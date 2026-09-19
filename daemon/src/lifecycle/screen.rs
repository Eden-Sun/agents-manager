//! Reading a terminal screen: cursor bookkeeping, echo stripping, noise and reply extraction.

use super::*;

/// Codex's account hint is outside any turn (not in `agent-turn-complete`); the terminal is the only source.
const CODEX_NOTICE_DELAY: Duration = Duration::from_millis(500);

/// Best-effort delayed read of Codex's hint; re-checks the run id so an old read can't land on a new run.
pub fn schedule_codex_notice_capture(app: &Arc<App>, bot_id: &str, run_id: &str) {
    let app = app.clone();
    let bot_id = bot_id.to_string();
    let run_id = run_id.to_string();
    tokio::spawn(async move {
        tokio::time::sleep(CODEX_NOTICE_DELAY).await;
        let lock = app.bot_lock(&bot_id).await;
        let _g = lock.lock().await;
        if let Err(e) = capture_codex_usage_notices(&app, &bot_id, &run_id).await {
            tracing::debug!(bot = %bot_id, run = %run_id, error = ?e, "codex notice capture failed");
        }
    });
}

/// Persist newly seen Codex account notices (reset available or hard limit). Caller holds the bot lock.
///
/// 撞限記不進正確那一格（讀不到主機、身分表還沒偵測完、排著的蓋不上憑據）時欠著（[`crate::turn_error::mark_codex_limit_hit`]，
/// #198），回合照樣收，錯誤最後回給呼叫端。
pub async fn capture_codex_usage_notices(app: &Arc<App>, bot_id: &str, expected_run_id: &str) -> anyhow::Result<()> {
    let Some(run) = db::active_run(&app.db, bot_id).await? else { return Ok(()) };
    if run.id != expected_run_id {
        return Ok(());
    }
    let Some(bot) = db::bot(&app.db, bot_id).await? else { return Ok(()) };
    if bot.kind != "codex" {
        return Ok(());
    }
    let Some(pane_id) = run.pane_id.as_deref() else { return Ok(()) };
    let Some(client) = app.herdr_for_run(&run).await else { return Ok(()) };
    let read = client.pane_read(pane_id, "recent_unwrapped", 200).await?;
    let conversation_id = db::conversation_id(&app.db, bot_id).await?;

    let notices = codex_usage_notice_lines(&read.text);
    let limit_banners: Vec<String> = notices.iter().filter(|n| codex_limit_hit_line(n).is_some()).cloned().collect();
    // 同一畫面裡比最後一張橫幅更新的狀態列還有餘裕 → 畫面上的撞限橫幅都是舊的（見 `limit_banner`）。
    let headroom_below = super::limit_banner::status_line_says_headroom(&read.text);
    let mut marked = Ok(());
    for notice in notices {
        let is_limit = codex_limit_hit_line(&notice).is_some();
        // 撞限要看的「有沒有回合在飛」在寫通知訊息**之前**讀（#198）：訊息寫了之後這一則就不再是新的（`fresh`），
        // 讀在後面的話讀錯就回錯，沒有在飛回合時下一次被當成看過的舊橫幅跳過，撞限永遠不記。
        let in_flight = if is_limit { db::in_flight_turn(&app.db, &run.id).await? } else { None };
        if is_limit {
            // fork／resume 重播的舊橫幅、或同一畫面更新的狀態列說還有額度：不寫系統訊息、不標額度、
            // 不解開回合（2026-09-14 AGM：fork 重播讓交辦被 quota_blocked）。`sighting` 每次讀取都要問，
            // 它同時更新「上一次看到幾次」。
            let seen = super::limit_banner::sighting(&run.id, &read.text, &notice, &limit_banners);
            let replayed = super::limit_banner::is_history(seen, in_flight.is_some());
            if replayed || headroom_below {
                tracing::debug!(bot = %bot.name, replayed, headroom_below, "codex limit banner on screen is history, not a limit hit");
                continue;
            }
        }
        // 去重只看這個 run 開始之後：比對整段對話時，兩天前一樣的上限橫幅讓這次被跳過，
        // 額度沒標、回合沒解開（2026-09-12 使用者）。
        let exists: i64 = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM messages
             WHERE conversation_id=? AND role='system' AND source='system' AND content=? AND created_at >= ?)",
        )
        .bind(&conversation_id)
        .bind(&notice)
        .bind(&run.started_at)
        .fetch_one(&app.db)
        .await?;
        let fresh = exists == 0;
        if fresh {
            // No pane snapshot: the idle splash is a boxed TUI, not a failed cut of a reply.
            insert_message(app, &conversation_id, None, "system", &notice, "system", false, None).await?;
            tracing::info!(bot = %bot.name, notice = %notice, "codex account notice captured");
        }
        #[cfg(test)]
        super::race_point::hit("codex_notice_after_insert", bot_id).await;
        if is_limit {
            // 有回合在飛＝橫幅就是那句的答案，照樣處理；否則只認這個 run 內第一次看到的，
            // 舊橫幅才不會反覆把額度打回 100%。
            if !fresh && in_flight.is_none() {
                continue;
            }
            // 記在這顆 bot 的主機與身分那一格；讀不到主機不退回 `local`（那會把本機帳號標成用盡，遠端那個用盡的身分
            // 反而沒擋），記不進去就欠著，派送前與 flush 照欠著的那一筆擋（#198）。
            if let Err(e) = crate::turn_error::mark_codex_limit_hit(app, &bot, &notice).await {
                marked = Err(e);
            }
            // Unlock the composer: a limit hit is a failed turn, not a silent idle.
            if let Some(turn) = in_flight {
                let res = super::turn_controller::fail(&app.db, &turn.id, super::turn_controller::DeliveryOnFail::Keep, "撞限橫幅").await?;
                if res == super::turn_controller::Outcome::Applied {
                    emit_turn(app, &turn.id).await;
                }
            }
        }
    }
    marked
}


pub(crate) async fn conversation_message_count(app: &Arc<App>, conversation_id: &str) -> anyhow::Result<i64> {
    Ok(sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM messages WHERE conversation_id = ?")
        .bind(conversation_id)
        .fetch_one(&app.db)
        .await?)
}

/// Newest assistant message: duplicate guard for re-reading an unchanged screen. 讀不到回錯（#193）：
/// 當成「還沒有回覆」，同一份回覆就存第二次。
pub(crate) async fn last_assistant_content(app: &Arc<App>, conversation_id: &str) -> anyhow::Result<Option<String>> {
    Ok(sqlx::query_scalar::<_, String>(
        "SELECT content FROM messages WHERE conversation_id = ? AND role = 'assistant'
         ORDER BY created_at DESC, id DESC LIMIT 1",
    )
    .bind(conversation_id)
    .fetch_optional(&app.db)
    .await?)
}

/// Record how far into the pane we have read, so the next capture starts after it.
pub(crate) async fn remember_pane_cursor(app: &Arc<App>, run_id: &str, read: &crate::herdr::PaneRead) -> anyhow::Result<()> {
    sqlx::query("UPDATE runs SET last_read_revision=?, last_read_tail_hash=? WHERE id=?")
        .bind(read.revision as i64)
        .bind(tail_hash(&read.text))
        .bind(run_id)
        .execute(&app.db)
        .await?;
    Ok(())
}

pub(crate) async fn remember_pane_cursor_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    run_id: &str,
    read: &crate::herdr::PaneRead,
) -> anyhow::Result<()> {
    sqlx::query("UPDATE runs SET last_read_revision=?, last_read_tail_hash=? WHERE id=?")
        .bind(read.revision as i64)
        .bind(tail_hash(&read.text))
        .bind(run_id)
        .execute(&mut **tx)
        .await?;
    Ok(())
}

fn tail_hash(text: &str) -> String {
    let tail: String = text.chars().rev().take(400).collect::<Vec<_>>().into_iter().rev().collect();
    format!("{:x}", md5ish(&tail))
}

fn md5ish(s: &str) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in s.as_bytes() {
        h ^= *b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

pub(crate) fn slice_after_cursor(text: &str, prev_tail_hash: Option<&str>) -> String {
    let Some(prev) = prev_tail_hash else { return text.to_string() };
    let chars: Vec<char> = text.chars().collect();
    for end in (0..=chars.len()).rev() {
        let start = end.saturating_sub(400);
        let window: String = chars[start..end].iter().collect();
        if format!("{:x}", md5ish(&window)) == prev {
            return chars[end..].iter().collect();
        }
    }
    text.to_string()
}

/// Prompt-echo prefix per CLI; single source for `after_last_prompt_echo` / `last_prompt_echo_text`.
pub(crate) fn prompt_echo_prefix(kind: &str) -> Option<&'static str> {
    match kind {
        "claude" | "grok" => Some("❯ "),
        "codex" => Some("› "),
        _ => None,
    }
}

/// Index after the last prompt echo (`❯ …` / `› …`), or 0 when not on screen.
pub(crate) fn after_last_prompt_echo(kind: &str, lines: &[&str]) -> usize {
    let Some(echo) = prompt_echo_prefix(kind) else { return 0 };
    lines
        .iter()
        .rposition(|l| {
            let t = l.trim_start();
            t.starts_with(echo) && t.len() > echo.len() && !is_codex_idle_prompt(t)
        })
        .map(|i| i + 1)
        .unwrap_or(0)
}

/// What the user typed on the last prompt echo, prefix stripped — the only source of the user
/// message for a turn typed straight into the pane (no hook payload until it ends).
pub fn last_prompt_echo_text(kind: &str, text: &str) -> Option<String> {
    let echo = prompt_echo_prefix(kind)?;
    let lines: Vec<&str> = text.lines().collect();
    let idx = after_last_prompt_echo(kind, &lines);
    if idx == 0 {
        return None;
    }
    let raw = lines[idx - 1];
    // grok right-aligns a clock and a scrollbar glyph onto the prompt row (see `clean_screen`).
    let stripped;
    let line = if kind == "grok" {
        stripped = strip_grok_decor(raw);
        stripped.as_str()
    } else {
        raw
    };
    let body = line.trim_start().strip_prefix(echo)?.trim();
    if body.is_empty() {
        return None;
    }
    // 折行的續行也是同一句話（2026-09-12 使用者回報）：只讀 `❯` 那行的話訊息被截斷，
    // 剩下半句被 `extract_reply` 當成 agent 的回覆。
    let mut out = vec![body.to_string()];
    // 只有排到行尾的回音才可能有續行；否則 grok 回覆、codex `thinking…` 這類縮排行會被誤收成使用者的話。
    if echo_row_is_full(raw) {
        for l in lines.iter().skip(idx) {
            match echo_continuation(kind, l) {
                Some(rest) => out.push(rest.to_string()),
                None => break,
            }
        }
    }
    Some(out.join("\n"))
}

/// 這行有沒有排到行尾（下一行可能是折下來的）。快照不知 pane 寬度，用顯示寬度（CJK 算兩欄）估，
/// 門檻 60 欄：實機折行都在 100 欄以上。
fn echo_row_is_full(raw: &str) -> bool {
    const WRAP_MIN_COLS: usize = 60;
    raw.trim_end().chars().map(|c| if (c as u32) > 0x1100 { 2 } else { 1 }).sum::<usize>() >= WRAP_MIN_COLS
}

/// 上一行回音的續行？續行＝有縮排且不是別的東西（`⎿`、`⏺`、`●`、`✻`、框線要先排除）。
/// 寧可少收（留一句回音）也不多收（吃掉 agent 的回覆）。
fn echo_continuation<'a>(kind: &str, line: &'a str) -> Option<&'a str> {
    // 續行一定有縮排；沒縮排的是下一塊內容。
    let rest = line.strip_prefix("  ")?;
    let t = rest.trim();
    if t.is_empty() || is_noise(line) {
        return None;
    }
    // 這些開頭代表另一塊東西開始了。
    const MARKERS: [&str; 10] = ["⎿", "⏺", "●", "✻", "✳", "│", "└", "├", "╭", "╰"];
    if MARKERS.iter().any(|m| t.starts_with(m)) {
        return None;
    }
    // 下一個回音行（使用者連送兩句）也不是續行。
    if let Some(echo) = prompt_echo_prefix(kind) {
        if t.starts_with(echo) {
            return None;
        }
    }
    Some(t)
}

/// Is this line TUI chrome (banner, boxes, rules, status bar, spinner) rather than content?
pub(crate) fn is_noise(s: &str) -> bool {
    crate::capture::claude::PARSER.noise_line(s)
}

/// Codex empty-composer placeholder `› Ask Codex to do anything` — looks like an echo, isn't one.
fn is_codex_idle_prompt(s: &str) -> bool {
    let t = s.trim_start();
    let body = t.strip_prefix("› ").unwrap_or(t).trim();
    body.to_ascii_lowercase().starts_with("ask codex to do")
}

/// codex 的經過時間：`45s`、`2m 5s`／`2m 05s`、`1h 2m 3s`（`fmt_elapsed_compact`、完成行的 `Worked for`）。
pub(crate) fn is_codex_duration(d: &str) -> bool {
    let parts: Vec<&str> = d.split(' ').collect();
    parts.len() <= 3
        && parts.iter().all(|p| {
            let (n, unit) = p.split_at(p.len().saturating_sub(1));
            !n.is_empty() && n.chars().all(|c| c.is_ascii_digit()) && matches!(unit, "h" | "m" | "s")
        })
}

/// codex 0.155 起回合成功結束後，回覆**下面**多一行完成時間（#207）：`Worked for 2m 5s · done 3:24 PM`，一分鐘以內只有
/// `done 3:24 PM`；不是今天就帶日期（`done Sep 6 at 2:32 PM`、`done Sep 6, 2000 at 2:32 PM`）；重播時可能只有 `Worked for …`；
/// 後面可能再接 ` · <runtime metrics>`。格式照 codex `history_cell/separators.rs`（rust-v0.155.0）。這是 TUI 的 chrome：
/// 不剝掉的話，備援切出來的回覆尾巴會多一行時間戳。0.154 以前的分隔線是整條 `─ Worked for … ─`、畫在回覆上面。
pub(crate) fn is_codex_completion_line(s: &str) -> bool {
    let s = s.trim();
    let rest = match s.strip_prefix("Worked for ") {
        Some(r) => {
            let (elapsed, tail) = r.split_once(" · ").unwrap_or((r, ""));
            if !is_codex_duration(elapsed) {
                return false;
            }
            if tail.is_empty() {
                return true;
            }
            tail
        }
        None => s,
    };
    let Some(when) = rest.strip_prefix("done ") else { return false };
    is_codex_done_time(when.split(" · ").next().unwrap_or(when))
}

/// `3:24 PM`／`Sep 6 at 2:32 PM`／`Sep 6, 2000 at 2:32 PM`（`%-I:%M %p`，別天加 `%b %-d`，別年再加 `, %Y`）。
fn is_codex_done_time(w: &str) -> bool {
    let clock = match w.split_once(" at ") {
        Some((date, clock)) => {
            let mut it = date.split(' ');
            let (Some(mon), Some(day)) = (it.next(), it.next()) else { return false };
            let year = it.next();
            let day_ok = match year {
                None => day.parse::<u32>().is_ok(),
                Some(y) => day.strip_suffix(',').is_some_and(|d| d.parse::<u32>().is_ok()) && y.len() == 4 && y.parse::<u32>().is_ok(),
            };
            if it.next().is_some() || mon.len() != 3 || month_num_token(mon).is_none() || !day_ok {
                return false;
            }
            clock
        }
        None => w,
    };
    let Some((hm, ampm)) = clock.split_once(' ') else { return false };
    let Some((h, m)) = hm.split_once(':') else { return false };
    matches!(ampm, "AM" | "PM")
        && h.parse::<u32>().is_ok_and(|h| (1..=12).contains(&h))
        && m.len() == 2
        && m.parse::<u32>().is_ok_and(|m| m < 60)
}

/// 回覆尾端的完成時間行（跟它前面的空行）拿掉。只剝**尾巴**：回覆中間剛好有一行長得一樣的字（codex 自己的測試就有），照留。
fn drop_codex_completion_tail(out: &mut Vec<String>) {
    loop {
        match out.last() {
            Some(l) if l.trim().is_empty() || is_codex_completion_line(l) => {
                let done = !l.trim().is_empty();
                out.pop();
                if done {
                    while out.last().is_some_and(|l| l.trim().is_empty()) {
                        out.pop();
                    }
                    return;
                }
            }
            _ => return,
        }
    }
}

/// grok 1.0.13 TUI chrome (appendix F): `◆` rows, "Worked for" footer, telemetry banner, shortcut
/// footer, `<cwd>   15K / 500K` header, `[stable]`.
fn is_grok_noise(s: &str) -> bool {
    if s.starts_with('◆') || s.starts_with("Worked for ") || s.contains("[hooks:") {
        return true;
    }
    if s.starts_with("Help improve Grok")
        || s.starts_with("Off by default.")
        || s == "settings."
        || s.starts_with("Read Terms and Privacy")
        || s == "[stable]"
        || s.starts_with("Grok Build ")
    {
        return true;
    }
    if s.contains("Ctrl+.:shortcuts") || s.contains("Shift+Tab:mode") || s.contains("Esc:cancel") {
        return true;
    }
    // "<cwd>                       15K / 500K"
    if let Some((_, tail)) = s.rsplit_once("  ") {
        let t = tail.trim();
        if t.ends_with('K') && t.contains(" / ") && t.chars().all(|c| c.is_ascii_digit() || c == 'K' || c == ' ' || c == '/' || c == '.') {
            return true;
        }
    }
    false
}

/// Strip grok's right-edge scrollbar `█` and right-aligned `h:mm AM|PM` clock.
pub(crate) fn strip_grok_decor(line: &str) -> String {
    let mut s = line.trim_end().trim_end_matches('█').trim_end().to_string();
    if let Some(rest) = s.strip_suffix(" AM").or_else(|| s.strip_suffix(" PM")) {
        if let Some((head, clock)) = rest.rsplit_once(' ') {
            let ok = clock.len() >= 4
                && clock.len() <= 5
                && clock.chars().filter(|c| *c == ':').count() == 1
                && clock.chars().all(|c| c.is_ascii_digit() || c == ':');
            // Two or more spaces before the clock = right-aligned column, not prose.
            if ok && head.ends_with(' ') {
                s = head.trim_end().to_string();
            }
        }
    }
    s
}

/// Codex usage-reset hint line; its glyph (`•` / `■`, varies by release) is not stored.
fn codex_usage_notice_line(line: &str) -> Option<String> {
    let body = line
        .trim()
        .strip_prefix('•')
        .or_else(|| line.trim().strip_prefix('■'))
        .map(str::trim_start)
        .unwrap_or_else(|| line.trim());
    let lower = body.to_ascii_lowercase();
    if lower.starts_with("you have ")
        && lower.contains("usage limit reset")
        && lower.contains("available")
        && lower.contains("run /usage")
    {
        Some(body.to_string())
    } else {
        None
    }
}

/// `ERROR: You've hit your usage limit. Upgrade to Pro …, or try again at Aug 8th, 2025 1:47 PM.`
pub(crate) fn codex_limit_hit_line(line: &str) -> Option<String> {
    let raw = line.trim();
    if raw.is_empty() {
        return None;
    }
    let s = match raw.chars().next() {
        Some(c) if !c.is_alphanumeric() => raw[c.len_utf8()..].trim(),
        _ => raw,
    };
    let body = s
        .strip_prefix("ERROR:")
        .or_else(|| s.strip_prefix("Error:"))
        .or_else(|| s.strip_prefix("error:"))
        .map(str::trim_start)
        .unwrap_or(s);
    let lower = body.to_ascii_lowercase();
    let hit = lower.contains("hit your usage limit")
        || (lower.contains("usage limit") && (lower.contains("try again") || lower.contains("upgrade to")));
    if !hit {
        return None;
    }
    // Stable ERROR: prefix so the chat styles it as a hard failure.
    if s.to_ascii_lowercase().starts_with("error:") {
        Some(s.to_string())
    } else {
        Some(format!("ERROR: {body}"))
    }
}

fn strip_codex_bullet(line: &str) -> &str {
    line.trim()
        .strip_prefix('•')
        .or_else(|| line.trim().strip_prefix('■'))
        .map(str::trim_start)
        .unwrap_or_else(|| line.trim())
}

/// Rejoin the limit banner that narrow panes wrap across rows, from row `i`; returns where it ends.
fn join_wrapped_limit_hit(lines: &[&str], i: usize) -> (Option<String>, usize) {
    let mut parts: Vec<&str> = vec![strip_codex_bullet(lines[i])];
    let mut j = i + 1;
    while j < lines.len() && parts.len() < 16 {
        let t = lines[j].trim();
        if t.is_empty() || t.starts_with('›') || t.starts_with('❯') || t.starts_with('╭') || t.starts_with('╰') {
            break;
        }
        // Model-picker chrome under the input box — stop.
        if t.to_ascii_lowercase().starts_with("gpt-") || t.contains("max fas") {
            break;
        }
        parts.push(t);
        j += 1;
    }
    (codex_limit_hit_line(&parts.join(" ")), j)
}

pub(crate) fn codex_usage_notice_lines(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let push = |out: &mut Vec<String>, notice: String| {
        if !out.iter().any(|seen| seen == &notice) {
            out.push(notice);
        }
    };
    let lines: Vec<&str> = text.lines().collect();
    let mut i = 0;
    while i < lines.len() {
        let line = lines[i];
        if let Some(notice) = codex_limit_hit_line(line) {
            // When wrapped, `try again at …` (the only reset time) is on the next row; the single row
            // stored `…, visit` with no reset (2026-09-10, codex-astra). Prefer the join only if it adds that.
            let (joined, next) = join_wrapped_limit_hit(&lines, i);
            if !has_try_again(&notice) {
                if let Some(joined) = joined.filter(|j| has_try_again(j)) {
                    push(&mut out, joined);
                    i = next;
                    continue;
                }
            }
            push(&mut out, notice);
            i += 1;
            continue;
        }
        if let Some(notice) = codex_usage_notice_line(line) {
            push(&mut out, notice);
            i += 1;
            continue;
        }
        let head_low = strip_codex_bullet(line).to_ascii_lowercase();
        let looks_hit = head_low.contains("hit your usage")
            || (head_low.contains("error") && head_low.contains("usage"))
            || head_low.contains("you've hit");
        if looks_hit {
            let (joined, next) = join_wrapped_limit_hit(&lines, i);
            if let Some(notice) = joined {
                push(&mut out, notice);
                i = next;
                continue;
            }
        }
        i += 1;
    }
    out
}

fn has_try_again(notice: &str) -> bool {
    notice.to_ascii_lowercase().contains("try again at")
}

fn month_num_token(tok: &str) -> Option<u32> {
    const M: [&str; 12] =
        ["jan", "feb", "mar", "apr", "may", "jun", "jul", "aug", "sep", "oct", "nov", "dec"];
    let t = tok.trim_matches(|c: char| !c.is_alphabetic()).to_ascii_lowercase();
    if t.len() < 3 {
        return None;
    }
    M.iter().position(|m| t.starts_with(m)).map(|i| i as u32 + 1)
}

/// `try again at Aug 8th, 2025 1:47 PM` → RFC3339 UTC. The date is optional: same-day resets are
/// a bare `5:07 AM.` (2026-09-10, codex-astra), read as the next time that clock comes round.
pub(crate) fn parse_codex_try_again(notice: &str) -> Option<String> {
    parse_codex_try_again_at(notice, chrono::Local::now())
}

/// 橫幅時間剛過去幾分鐘＝舊橫幅，不要滾到明天。2026-09-13：22:15:22 派工時橫幅還是 `10:15 PM`，
/// 滾成隔天讓兩筆交辦等 24 小時，而 app-server 說 22:20 就重置。
const STALE_BANNER_GRACE_MINS: i64 = 15;
const STALE_BANNER_RETRY_MINS: i64 = 5;

/// codex 只在「重置就在**同一個當地日期**」時省略日期（0.154.0：同日 `%-I:%M %p`，跨日
/// `%b %-d<th>, %Y %-I:%M %p`）。所以裸鐘點永遠是「今天的那個時刻」：
///
/// * 已經過去（不管多久）＝畫面上留著的舊橫幅，改成幾分鐘後再問（2026-09-14 使用者：08:53 讀到
///   寫著 `3:22 AM` 的舊橫幅，被滾成隔天 03:22，平白鎖 24 小時）。
/// * 還在未來就照字面，**不設上限**：今天稍晚才重置的週窗或 credits（`11:40 PM`）以前被 6 小時
///   上限改寫成「5 分鐘後再問」，真的撞限只擋 5 分鐘，然後每 5 分鐘重送一次直到 quota_exhausted
///   （review3 c2 M1）。

/// 可測版本：`now` 由呼叫端給。
fn parse_codex_try_again_at(notice: &str, now: chrono::DateTime<chrono::Local>) -> Option<String> {
    use chrono::{Datelike, Local, NaiveDate, TimeZone};
    let low = notice.to_ascii_lowercase();
    let rest = low.split("try again at").nth(1)?.trim();
    let mut month = None;
    let mut day = None;
    let mut year = None;
    let mut hour = None;
    let mut minute = 0u32;
    let mut pm = false;
    for tok in rest.split(|c: char| c.is_whitespace() || c == ',').filter(|t| !t.is_empty()) {
        if month.is_none() {
            if let Some(m) = month_num_token(tok) {
                month = Some(m);
                continue;
            }
        }
        let digits: String = tok.chars().take_while(|c| c.is_ascii_digit()).collect();
        if digits.is_empty() {
            // The last token is `pm.`: an exact match loses the meridiem (reset 12 h early).
            match tok.trim_matches(|c: char| !c.is_ascii_alphanumeric()) {
                "pm" => pm = true,
                "am" => pm = false,
                _ => {}
            }
            continue;
        }
        if let Some((h, m)) = tok.split_once(':') {
            if let (Ok(h), Ok(m)) = (
                h.chars().take_while(|c| c.is_ascii_digit()).collect::<String>().parse::<u32>(),
                m.chars().take_while(|c| c.is_ascii_digit()).collect::<String>().parse::<u32>(),
            ) {
                hour = Some(h);
                minute = m;
                let tail = tok.to_ascii_lowercase();
                if tail.contains("pm") {
                    pm = true;
                } else if tail.contains("am") {
                    pm = false;
                }
                continue;
            }
        }
        match digits.len() {
            4 => year = digits.parse().ok(),
            _ if day.is_none() => day = digits.parse().ok(),
            _ if hour.is_none() => hour = digits.parse().ok(),
            _ => {}
        }
        if tok.to_ascii_lowercase().ends_with("pm") {
            pm = true;
        } else if tok.to_ascii_lowercase().ends_with("am") {
            pm = false;
        }
    }
    let mut hour = hour?;
    if pm && hour < 12 {
        hour += 12;
    }
    if !pm && hour == 12 {
        hour = 0;
    }
    // A reset is always ahead: a passed clock time means tomorrow, a passed month/day next year.
    let today = now.date_naive();
    let (date, roll) = match (month, day) {
        (Some(m), Some(d)) => (NaiveDate::from_ymd_opt(year.unwrap_or_else(|| today.year()), m, d)?, year.is_none()),
        _ => (today, true),
    };
    let mut naive = date.and_hms_opt(hour, minute, 0)?;
    let bare_clock = month.is_none() || day.is_none();
    if roll && naive <= now.naive_local() {
        if bare_clock {
            // 裸鐘點只會是「今天」（見上面的說明）：已經過去就是舊橫幅，晚點再問，不滾到明天。
            naive = now.naive_local() + chrono::Duration::minutes(STALE_BANNER_RETRY_MINS);
        } else {
            // 帶月日、沒有年份：剛過去幾分鐘＝舊橫幅，晚點再問；過很久才是去年的同一天，滾到明年。
            let behind = now.naive_local().signed_duration_since(naive);
            naive = if behind <= chrono::Duration::minutes(STALE_BANNER_GRACE_MINS) {
                now.naive_local() + chrono::Duration::minutes(STALE_BANNER_RETRY_MINS)
            } else {
                naive.with_year(naive.year() + 1)?
            };
        }
    }
    let dt = Local.from_local_datetime(&naive).earliest()?;
    Some(dt.with_timezone(&chrono::Utc).to_rfc3339_opts(chrono::SecondsFormat::Millis, true))
}

/// codex 撞限橫幅的那一筆撞限：到期是橫幅上寫的時間（撞的當下解析），沒寫就沒有（credits 用完，等下一個成功回合清）。
/// 正式路徑在 `turn_error::mark_codex_limit_hit` 組（欠帳要留著撞的當下那一份），這支給測試直接寫一格。
#[cfg(test)]
pub(crate) fn codex_limit_hit(notice: &str, at: String) -> crate::quota::LimitHit {
    crate::quota::LimitHit { message: notice.to_string(), until: parse_codex_try_again(notice), at, bucket: None }
}

/// Mirror a Codex hard limit-hit onto that host's quota row immediately (the rate-limits RPC lags).
/// 回傳記憶體裡擋著的那一筆（同一張還沒過期的橫幅就是原本那筆）。
///
/// `base` 由呼叫端算（[`crate::turn_error::mark_codex_limit_hit`]）：寫進這顆 bot 身分的 key——查詢端先查
/// `codex:<identity>`，以前寫裸 `codex` 對不上（2026-09-13 AGM）；讀不到主機、身分表還沒偵測完都不猜（#198）。
pub(crate) async fn apply_codex_limit_hit_quota(app: &Arc<App>, host: &str, base: &str, hit: crate::quota::LimitHit) -> crate::quota::LimitHit {
    let key = crate::quota::quota_key(host, base);
    let mut q = app
        .quotas
        .lock()
        .await
        .get(&key)
        .cloned()
        .unwrap_or_else(|| crate::quota::Quota {
            five_hour: None,
            seven_day: None,
            fable: None,
            reset_credits: None,
            limit_hit: None,
            plan: None,
            updated_at: crate::db::now(),
            source: "codex-limit-hit".into(),
            account: None,
            host: host.to_string(),
        });
    // 同一張橫幅再看到不是新證據（2026-09-13：掃到 22:15 的舊橫幅卻把 `at` 蓋成現在、量表打回 100%）。
    // 但**已經過期**的那張不算數：`until` 到了、交辦重送、CLI 回同一句橫幅——這是真的又被擋一次，
    // 略過的話 `on_turn_done` 查不到撞限，交辦會被結成 failed 送去驗收（review3 c2 M1）。
    if let Some(h) = q.limit_hit.as_ref().filter(|h| h.message == hit.message && !crate::quota::limit_hit_expired(Some(h))) {
        return h.clone();
    }
    // 量表標成用完，但重置時間不從橫幅寫：橫幅時間會舊會歪（同日解析成隔天，交辦等 24 小時）。
    // 只記在 `limit_hit.until`；`resets_at` 留給 app-server／statusLine。
    let win = crate::quota::Window { used_pct: 100.0, resets_at: None };
    if let Some(existing) = q.five_hour.as_mut() {
        existing.used_pct = 100.0;
    } else if let Some(existing) = q.seven_day.as_mut() {
        existing.used_pct = 100.0;
    } else {
        q.five_hour = Some(win);
    }
    // 黏著走，直到 `until` 過了、下一回合成功、或更新的結構化讀數說還有額度（`quota::set`）。
    q.limit_hit = Some(hit.clone());
    q.updated_at = crate::db::now();
    q.source = "codex-limit-hit".into();
    crate::quota::set(app, host, base, q).await;
    hit
}

/// No reply marker: keep what follows the last prompt echo minus chrome. `⎿` lines stay — they
/// usually carry the actual error ("Not logged in · Please run /login").
pub(crate) fn clean_screen(kind: &str, text: &str) -> Option<String> {
    let lines: Vec<&str> = text.lines().collect();
    let start = after_last_prompt_echo(kind, &lines);
    let mut out: Vec<String> = Vec::new();
    let grok = kind == "grok";
    // grok's telemetry banner wraps at pane width: skip "Help improve Grok" … "Privacy Policy." as a block.
    let mut in_banner = false;
    for line in &lines[start..] {
        let stripped;
        let s = if grok {
            stripped = strip_grok_decor(line);
            stripped.trim()
        } else {
            line.trim()
        };
        if grok {
            if s.starts_with("Help improve Grok") {
                in_banner = true;
            }
            if in_banner {
                if s.starts_with("Read Terms and Privacy") {
                    in_banner = false;
                }
                continue;
            }
        }
        // `is_activity_shape` catches spinner glyphs `is_noise` doesn't know.
        if is_noise(s) || is_activity_shape(s) || (grok && is_grok_noise(s)) {
            continue;
        }
        // An empty prompt box means the transcript ended.
        if s == "❯" || s == "›" {
            break;
        }
        let s = s.strip_prefix("⎿ ").or_else(|| s.strip_prefix("⎿")).unwrap_or(s).trim();
        if s.is_empty() {
            if !out.last().map(|l: &String| l.is_empty()).unwrap_or(true) {
                out.push(String::new());
            }
            continue;
        }
        out.push(s.to_string());
    }
    while out.last().map(|l| l.is_empty()).unwrap_or(false) {
        out.pop();
    }
    if kind == "codex" {
        drop_codex_completion_tail(&mut out);
    }
    let joined = out.join("\n").trim().to_string();
    if joined.is_empty() {
        None
    } else {
        Some(joined)
    }
}

/// Provider-specific reply extraction. grok has no reply marker (appendix F): always `clean_screen`.
pub(crate) fn extract_reply(kind: &str, text: &str) -> Option<String> {
    if kind == "claude" {
        return crate::capture::claude::PARSER.extract_reply(text);
    }
    let marker = match kind {
        "codex" => "• ",
        _ => return None,
    };
    let lines: Vec<&str> = text.lines().collect();
    // A2: only this turn's output — else the previous turn's `⏺` line is returned as the answer.
    let after_echo = after_last_prompt_echo(kind, &lines);
    let start = after_echo
        + lines[after_echo..].iter().rposition(|l| {
            let s = l.trim_start();
            s.starts_with(marker) && (kind != "codex" || codex_usage_notice_line(s).is_none())
        })?;
    let mut out: Vec<String> = Vec::new();
    for line in &lines[start..] {
        let t = line.trim_end();
        let s = t.trim_start();
        // Stop at the input box / horizontal rule drawn below the transcript.
        if s.starts_with('╭') || s.starts_with('│') || s.starts_with('╰') || s.starts_with('▔') {
            break;
        }
        if !s.is_empty() && s.chars().all(|c| c == '─' || c == '━' || c == '-' || c == '=' || c == '_') {
            break;
        }
        // Skip the spinner / status line and its neighbours (`Tip:`, `✗ Auto-update failed`).
        if s.chars().next().map(is_spinner_glyph).unwrap_or(false) || is_noise(s) {
            continue;
        }
        let cleaned = s.strip_prefix(marker).unwrap_or(t).to_string();
        out.push(cleaned);
    }
    if kind == "codex" {
        drop_codex_completion_tail(&mut out);
    }
    while out.last().map(|l| l.trim().is_empty()).unwrap_or(false) {
        out.pop();
    }
    let joined = out.join("\n").trim().to_string();
    if joined.is_empty() {
        None
    } else {
        Some(joined)
    }
}

#[cfg(test)]
mod extract_tests {
    use super::*;

    const NOT_LOGGED_IN: &str = "\
 ▐▛███▛█   Claude Code v2.1.261
▝▜██████▀  Haiku 4.5 · API Usage Billing
  ▝▝ ▝▝    ~/project/hermes-agents/projects/pt

 ⚠ AGENTS.md is over the 40.0k-char limit (57.0k chars) · /memory to free up context

❯ echo 1
  ⎿  Not logged in · Please run /login
   · Run in another terminal: security unlock-keychain

✻ Worked for 0s · done 1:07 AM
────────────────────────────────────────────
❯
────────────────────────────────────────────
  tony. | pt | HAI4.5 | 5h:- | 7d:-
  ⏵⏵ bypass permissions on (shift+tab to cycle) · ← for agents
";

    /// issue #206：claude `agents-md` plugin 的兩種提示——2.1.276 在通知列跳 10 秒的 toast（「This project has AGENTS.md but
    /// no CLAUDE.md; …」），2.1.277 起改讀 AGENTS.md 時記一行 log（「no CLAUDE.md found; AGENTS.md loaded: …」）。畫面上的確切
    /// 位置要等伺服器端旗標放量後在真機抓（本機 2.1.278 旗標關著）；這裡放兩種：banner 下方（跟 `⚠ AGENTS.md is over …` 同一區）、
    /// 以及最容易出事的回覆與輸入框之間。
    const CLAUDE_AGENTS_MD_NOTICE: &str = "\
 ▐▛███▛█   Claude Code v2.1.277
▝▜██████▀  Opus 4.8 · Claude Max
  ▝▝ ▝▝    ~/project/demo

  no CLAUDE.md found; AGENTS.md loaded: /Users/u/project/demo/AGENTS.md
  This project has AGENTS.md but no CLAUDE.md; set the agents-md plugin's projectInstructions option to agents-fallback to load it.

❯ Reply with PONG please

⏺ PONG

────────────────────────────────────────────
❯
────────────────────────────────────────────
  u | demo | OPUS | 5h:90%
  ⏵⏵ bypass permissions on (shift+tab to cycle)
";

    /// 同一批提示，畫在回覆與輸入框之間（通知列的可能位置）。
    const CLAUDE_AGENTS_MD_TOAST_UNDER_REPLY: &str = "\
❯ Reply with PONG please

⏺ PONG

  This project has AGENTS.md but no CLAUDE.md; set the agents-md plugin's projectInstructions option to agents-fallback to load it.
  no CLAUDE.md found; AGENTS.md loaded: /Users/u/project/demo/AGENTS.md
────────────────────────────────────────────
❯
────────────────────────────────────────────
";

    /// 那兩行提示不是回覆內容、不是使用者打的字，也不是輸入框——不管畫在 banner 下方還是回覆下方。
    #[test]
    fn claude_agents_md_notices_are_neither_the_reply_nor_the_prompt() {
        for (where_, screen) in [("banner 下方", CLAUDE_AGENTS_MD_NOTICE), ("回覆下方", CLAUDE_AGENTS_MD_TOAST_UNDER_REPLY)] {
            assert_eq!(extract_reply("claude", screen).as_deref(), Some("PONG"), "{where_}");
            let cleaned = clean_screen("claude", screen).unwrap_or_default();
            assert!(!cleaned.contains("AGENTS.md"), "{where_}：{cleaned}");
            assert_eq!(last_prompt_echo_text("claude", screen).as_deref(), Some("Reply with PONG please"), "{where_}");
        }
    }

    const CODEX_STARTUP: &str = "\
╭────────────────────────────────────────────╮
│ >_ OpenAI Codex (v0.153.4)                  │
╰────────────────────────────────────────────╯

  Tip: Use /init to create an AGENTS.md.

• You have 1 usage limit reset available. Run /usage to use one.

› Write tests for @filename
";

    #[test]
    fn codex_usage_reset_hint_is_captured_without_the_bullet() {
        assert_eq!(
            codex_usage_notice_lines(CODEX_STARTUP),
            vec!["You have 1 usage limit reset available. Run /usage to use one."],
        );
        // Older Codex builds used a square marker and pluralised the noun.
        assert_eq!(
            codex_usage_notice_line(" ■ You have 2 usage limit resets available. Run /usage to use one."),
            Some("You have 2 usage limit resets available. Run /usage to use one.".into()),
        );
        assert!(codex_usage_notice_line("• ordinary assistant text").is_none());
    }

    /// Codex 0.153 idle splash (2026-09-08): must not become a user prompt or a reply.
    const CODEX_IDLE_SPLASH: &str = "\
╭────────────────────────────────────────────╮
│ >_ OpenAI Codex (v0.153.4)                  │
│                                            │
│ model:        gpt-5.6-luna max  fast   /model to change
│ directory:    ~/project/hermes-agents/projects/pt
│ permissions:  YOLO mode
╰────────────────────────────────────────────╯

  Tip: Type / to open the command popup; Tab autocompletes slash commands.

• You have 1 usage limit reset available. Run /usage to use one.

› Ask Codex to do anything

gpt-5.6-luna max fast · ~/project/hermes-agents/projects/pt · Context 0% used · 5h 100% left
";

    #[test]
    fn codex_idle_splash_is_not_a_reply_or_a_user_prompt() {
        assert_eq!(clean_screen("codex", CODEX_IDLE_SPLASH), None, "{:?}", clean_screen("codex", CODEX_IDLE_SPLASH));
        assert_eq!(extract_reply("codex", CODEX_IDLE_SPLASH), None);
        assert_eq!(last_prompt_echo_text("codex", CODEX_IDLE_SPLASH), None);
        assert!(codex_usage_notice_lines(CODEX_IDLE_SPLASH).iter().any(|n| n.contains("usage limit reset")));
    }

    const CODEX_LIMIT_HIT: &str = "\
ERROR: You've hit your usage limit. Upgrade to Pro (https://chatgpt.com/explore/pro), \
or try again at Aug 8th, 2025 1:47 PM.
";

    #[test]
    fn codex_limit_hit_is_captured_as_notice() {
        let lines = codex_usage_notice_lines(CODEX_LIMIT_HIT);
        assert_eq!(lines.len(), 1);
        assert!(lines[0].to_ascii_lowercase().contains("hit your usage limit"));
        assert!(lines[0].starts_with("ERROR:"));
        assert_eq!(
            codex_limit_hit_line(
                "ERROR: You've hit your usage limit. Upgrade to Pro, or try again at Sep 6th, 2026 3:00 PM."
            )
            .map(|s| s.contains("hit your usage limit")),
            Some(true),
        );
        assert!(codex_limit_hit_line("• ordinary assistant text").is_none());
    }

    /// Real pane when Codex wraps the banner at ~28 columns.
    const CODEX_LIMIT_HIT_WRAPPED: &str = "\
› ping

■ You've hit your usage
limit. Upgrade to Pro
(https://chatgpt.com/ex
plore/pro),
visit
https://chatgpt.com/cod
ex/settings/usage
to purchase more
credits or try again at
1:32 PM.

› Ask Codex to do anyt

  gpt-5.6-luna max fas…
";

    #[test]
    fn codex_limit_hit_survives_narrow_pane_wrap() {
        let lines = codex_usage_notice_lines(CODEX_LIMIT_HIT_WRAPPED);
        assert_eq!(lines.len(), 1, "{lines:?}");
        let n = lines[0].to_ascii_lowercase();
        assert!(n.contains("hit your usage"));
        assert!(n.contains("limit"));
        assert!(n.contains("try again"));
    }

    /// codex-astra 2026-09-10: even a wide pane wraps this banner; the reset half was dropped (`…, visit`).
    const CODEX_LIMIT_HIT_TWO_ROWS: &str = "\
› AGM 交辦：…

■ You've hit your usage limit. Upgrade to Pro (https://chatgpt.com/explore/pro), visit
https://chatgpt.com/codex/settings/usage to purchase more credits or try again at 5:07 AM.

  1 background terminal running · /ps to view · /stop to close
";

    #[test]
    fn codex_limit_hit_keeps_the_reset_half_off_the_next_row() {
        let lines = codex_usage_notice_lines(CODEX_LIMIT_HIT_TWO_ROWS);
        assert_eq!(lines.len(), 1, "{lines:?}");
        let n = lines[0].to_ascii_lowercase();
        assert!(n.contains("hit your usage limit"));
        assert!(n.contains("try again at 5:07 am"), "the reset half must survive: {n}");
        assert!(parse_codex_try_again(&lines[0]).is_some());
        // A banner that already carries its own reset is not extended by whatever follows it.
        assert_eq!(codex_usage_notice_lines(CODEX_LIMIT_HIT).len(), 1);
    }
    /// 2026-09-13：派工時橫幅剛過去 22 秒是舊字，不能滾到隔天壓 24 小時。
    #[test]
    fn a_banner_that_just_went_stale_does_not_roll_to_tomorrow() {
        use chrono::TimeZone;
        let now = chrono::Local.with_ymd_and_hms(2026, 9, 13, 22, 15, 22).unwrap();
        let got = parse_codex_try_again_at("ERROR: You've hit your usage limit, or try again at 10:15 PM.", now)
            .expect("讀得到時間");
        let t = chrono::DateTime::parse_from_rfc3339(&got).unwrap();
        let mins = (t.timestamp() - now.timestamp()) / 60;
        assert!((4..=6).contains(&mins), "應該是幾分鐘後再問，不是隔天：{got}（{mins} 分）");
    }

    /// 2026-09-14 使用者：08:53 讀到寫著 `3:22 AM` 的舊橫幅。裸鐘點過去很久不代表「明天那個時刻」
    /// ——codex 只在今天之內就會回來時寫裸鐘點——那是畫面上留著的舊字，改成幾分鐘後再問。
    #[test]
    fn a_clock_time_long_past_is_a_stale_banner_not_tomorrow() {
        use chrono::TimeZone;
        let now = chrono::Local.with_ymd_and_hms(2026, 9, 14, 8, 53, 3).unwrap();
        let got = parse_codex_try_again_at(
            "ERROR: You've hit your usage limit. Upgrade to Pro, visit …/usage to purchase more credits or try again at 3:22 AM.",
            now,
        )
        .expect("讀得到時間");
        let t = chrono::DateTime::parse_from_rfc3339(&got).unwrap();
        let mins = (t.timestamp() - now.timestamp()) / 60;
        assert!((4..=6).contains(&mins), "應該是幾分鐘後再問，不是明天 03:22（鎖 18 小時）：{got}（{mins} 分）");
    }

    /// 今天稍晚的鐘點照舊當真：那才是 5 小時視窗真的會回來的時間。
    #[test]
    fn a_clock_time_later_today_is_taken_at_face_value() {
        use chrono::{Local, TimeZone, Timelike};
        let now = chrono::Local.with_ymd_and_hms(2026, 9, 14, 8, 53, 3).unwrap();
        let got = parse_codex_try_again_at("or try again at 11:30 AM.", now).expect("讀得到時間");
        let dt = chrono::DateTime::parse_from_rfc3339(&got).unwrap().with_timezone(&Local);
        assert_eq!((dt.hour(), dt.minute()), (11, 30), "{got}");
        assert_eq!(dt.date_naive(), now.date_naive(), "今天，不是明天：{got}");
    }


    /// 裸鐘點永遠是「今天的那個時刻」：今天稍晚就照字面（不設上限），已經過去就是舊橫幅、幾分鐘後再問。
    #[test]
    fn codex_try_again_without_a_date_is_always_today() {
        use chrono::{Datelike, Local, TimeZone, Timelike};
        let now = Local::now();
        let at = |h: u32| {
            format!("ERROR: You've hit your usage limit. Upgrade to Pro, or try again at {}:07 {}.",
                if h % 12 == 0 { 12 } else { h % 12 },
                if h < 12 { "AM" } else { "PM" })
        };
        for h in 0..24u32 {
            // 同一個 `now` 解析：各讀各的時鐘時跨過邊界會偶發紅燈（2026-09-13）。
            let candidate = now.date_naive().and_hms_opt(h, 7, 0).unwrap();
            let parsed = parse_codex_try_again_at(&at(h), now).unwrap_or_else(|| panic!("hour {h} did not parse"));
            let dt = chrono::DateTime::parse_from_rfc3339(&parsed).unwrap().with_timezone(&Local);
            if candidate > now.naive_local() {
                // 今天稍晚：照字面，離現在多久都一樣（週窗、credits 可能是今天 23:40 才回來）。
                assert_eq!((dt.hour(), dt.minute()), (h, 7), "{parsed}");
                assert_eq!(dt.date_naive(), now.date_naive(), "今天，不是明天：{parsed}");
            } else {
                // 已經過去＝畫面上留著的舊橫幅：幾分鐘後再問，不滾到明天。
                let mins = dt.signed_duration_since(now).num_minutes();
                assert!((4..=6).contains(&mins), "hour {h} 應該是幾分鐘後再問：{parsed}（{mins} 分）");
            }
        }
        // A month/day with no year still lands on a real date.
        let r = parse_codex_try_again_at("try again at Aug 8th 1:47 PM.", now).unwrap();
        let dt = chrono::DateTime::parse_from_rfc3339(&r).unwrap().with_timezone(&Local);
        assert_eq!((dt.month(), dt.day()), (8, 8));
        assert!(dt > now);
        let _ = Local.timestamp_opt(0, 0);
        let _ = now.year();
    }

    /// review3 c2 M1：codex 的週額度／credits 今天稍晚才重置（`11:40 PM`）。以前「裸鐘點最多指到 6 小時之外」
    /// 把它改寫成「5 分鐘後再問」：真的撞限只擋 5 分鐘，接著每 5 分鐘重送一次，約半小時就燒完重試次數變成
    /// `quota_exhausted`。
    #[test]
    fn a_reset_later_today_that_is_far_away_is_still_taken_at_face_value() {
        use chrono::{Local, TimeZone, Timelike};
        let now = Local.with_ymd_and_hms(2026, 9, 16, 9, 0, 0).unwrap();
        let got = parse_codex_try_again_at(
            "ERROR: You've hit your usage limit. Upgrade to Pro, or try again at 11:40 PM.",
            now,
        )
        .expect("讀得到時間");
        let dt = chrono::DateTime::parse_from_rfc3339(&got).unwrap().with_timezone(&Local);
        assert_eq!((dt.hour(), dt.minute()), (23, 40), "{got}");
        assert_eq!(dt.date_naive(), now.date_naive(), "今天，不是 5 分鐘後：{got}");
    }

    #[test]
    fn codex_try_again_at_parses_reset() {
        let r = parse_codex_try_again(
            "ERROR: You've hit your usage limit. Upgrade to Pro, or try again at Aug 8th, 2025 1:47 PM.",
        )
        .unwrap();
        assert!(r.starts_with("2025-08-08T"), "{r}");
    }

    #[test]
    fn live_alert_surfaces_codex_limit_hit() {
        let alert = live_alert("codex", CODEX_LIMIT_HIT).unwrap();
        assert!(alert.to_ascii_lowercase().contains("hit your usage limit"));
    }

    #[test]
    fn clean_screen_keeps_only_tool_result_lines() {
        assert!(extract_reply("claude", NOT_LOGGED_IN).is_none());
        let got = clean_screen("claude", NOT_LOGGED_IN).unwrap();
        assert_eq!(got, "Not logged in · Please run /login");
    }

    /// Second turn printed only tool output; the previous `⏺ FIRST-ANSWER` must not be its answer (review A2).
    const TWO_TURNS: &str = "\
❯ echo 1
⏺ FIRST-ANSWER

❯ echo 2
  ⎿  Not logged in · Please run /login

✻ Worked for 0s
────────────────────────────────────────────
❯
";

    #[test]
    fn live_alert_catches_a_retry_banner() {
        let screen = "\
❯ do the thing
● Running 3 shell commands…
  ⎿ $ ls
✻ API error · Retrying in 0s · attempt 1/10";
        assert_eq!(
            live_alert("claude", screen).as_deref(),
            Some("API error · Retrying in 0s · attempt 1/10")
        );
        // codex words it differently; the shape is what matches.
        assert_eq!(
            live_alert("codex", "stream error: 503 upstream; retrying 2/5 in 1s").as_deref(),
            Some("stream error: 503 upstream; retrying 2/5 in 1s")
        );
        // The newest banner wins.
        let two = "API error · Retrying in 0s · attempt 1/10\nAPI error · Retrying in 4s · attempt 2/10";
        assert!(live_alert("claude", two).unwrap().ends_with("attempt 2/10"));
    }

    #[test]
    fn live_alert_ignores_the_agent_talking_about_errors() {
        // Prose that merely mentions an error is not a banner: no retry token.
        assert!(live_alert("claude", "I fixed the error in the parser.").is_none());
        // A retry token with no error is not one either.
        assert!(live_alert("claude", "Retrying the test suite now").is_none());
        // Long prose that happens to contain both stays out.
        let prose = format!("The {} error means we should retry the request later on.", "x".repeat(300));
        assert!(live_alert("claude", &prose).is_none());
        // Printed data saying error + attempt (sqlite row / JSON, 2026-09-08).
        let row = r#"62|note||{"action":"protocol_error","attempt":3,"bot":"t1-dev-2","error":"report.status 必須是 done 或 blocked"}|06:12"#;
        assert!(live_alert("claude", row).is_none());
        assert!(live_alert("claude", r#"{"error":"timeout","retry":true}"#).is_none());
        // …but a real banner still gets through.
        assert!(live_alert("claude", "API error · Retrying in 2s · attempt 2/10").is_some());
    }

    #[test]
    fn extract_reply_ignores_the_previous_turn() {
        assert_eq!(extract_reply("claude", TWO_TURNS), None);
        assert_eq!(clean_screen("claude", TWO_TURNS).unwrap(), "Not logged in · Please run /login");
    }

    #[test]
    fn extract_reply_prefers_marker() {
        let screen = "❯ Reply with PONG\n⏺ PONG\n✻ Cooked for 5s\n──────\n❯\n";
        assert_eq!(extract_reply("claude", screen).unwrap(), "PONG");
    }

    /// grok 1.0.13 `agent.read {source: visible}` (appendix F), columns narrowed.
    const GROK_SCREEN: &str = "\

  /private/tmp/scratch/grok-ws                                       15K / 500K


     ❯ Reply with exactly GROK-OK                                        2:09 AM
                                                                                █
     ◆ user_prompt_submit  [hooks: 1]                                           █
     ◆ Thought for 0.1s                                                         █
                                                                                █
     GROK-OK                                                             2:09 AM   █
                                                                                █
     Worked for 3.6s                                        stop  [hooks: 2]   █
                                                                                █

  Help improve Grok                                       [Opt out] [Opt in]
  Off by default. Opt-in to allow SpaceXAI to retain coding data. Change anytime via
  settings.
  Read Terms and Privacy Policy.

  ╭──────────────────────────────────────────────────────────────────────────╮
  │ ❯                                                                        │
  ╰──────────────────────────────── Grok 4.6 (low) · always-approve ─╯

  Shift+Tab:mode  │  Ctrl+.:shortcuts
";

    /// 空框判定吃各 provider 的實機畫面（sol 第八輪 #2）：這些 fixture 都是真的 pane 抓下來的。
    #[test]
    fn the_real_empty_composers_of_every_provider_are_empty() {
        use crate::lifecycle::{box_state, BoxState};
        // grok：`│ ❯   │` 裡的結構空白、`╰── Grok 4.6 (low) · always-approve ─╯` 框底。
        assert_eq!(box_state("grok", GROK_SCREEN), BoxState::Empty);
        assert_eq!(box_state("grok", GROK_SCREEN_NARROW), BoxState::Empty);
        assert_eq!(box_state("grok", GROK_AWAITING), BoxState::Empty, "窄到只剩 `╰─ Grok ─╯`");
        // codex：沒有框。純文字快照看不出佔位字是不是有人打的 → 保守當非空（sol 第九輪 #2）。
        assert_eq!(box_state("codex", CODEX_IDLE_SPLASH), BoxState::NonEmpty, "純文字的 › Ask Codex to do anything");
        assert_eq!(box_state("codex", CODEX_STARTUP), BoxState::NonEmpty, "純文字的 › Write tests for @filename");
        // 同一列用 ANSI 讀（2026-09-14 w168:p3E 實機）：佔位字被畫成 dim（SGR 2）→ 才是空框。
        let styled = CODEX_IDLE_SPLASH.replacen("› Ask Codex to do anything", CODEX_PLACEHOLDER_ANSI, 1);
        assert_eq!(box_state("codex", &styled), BoxState::Empty);
        // claude 舊版：`│ ❯ │` 框。
        assert_eq!(box_state("claude", THINKING_ONLY), BoxState::Empty);
        // claude 現行：兩條全寬框線中間的輸入列，裡面是使用者真的打的字 → 非空。
        assert_eq!(box_state("claude", CLAUDE_UNSENT_PROMPT), BoxState::NonEmpty);
    }

    /// w168:p3E（codex 0.154，2026-09-14）用 `pane.read format=ansi` 讀到的輸入列原樣。
    const CODEX_PLACEHOLDER_ANSI: &str = "\u{1b}[0m\u{1b}[1m\u{1b}[48;2;59;64;76m›\u{1b}[0m\u{1b}[48;2;59;64;76m \u{1b}[0m\u{1b}[2m\u{1b}[48;2;59;64;76mAsk Codex to do anything\u{1b}[0m\u{1b}[48;2;59;64;76m   \u{1b}[0m";

    /// 跟佔位字一字不差、但不是 dim 的字＝使用者打的：非空（sol 第九輪 #2）。
    #[test]
    fn typed_text_identical_to_a_placeholder_is_not_empty() {
        use crate::lifecycle::{box_state, BoxState};
        let typed = "\u{1b}[0m\u{1b}[1m›\u{1b}[0m Ask Codex to do anything";
        let s = CODEX_IDLE_SPLASH.replacen("› Ask Codex to do anything", typed, 1);
        assert_eq!(box_state("codex", &s), BoxState::NonEmpty);
        // 一半 dim、一半不是：也不算佔位字。
        let half = "›\u{1b}[0m \u{1b}[2mAsk Codex\u{1b}[22m to do anything";
        let s = CODEX_IDLE_SPLASH.replacen("› Ask Codex to do anything", half, 1);
        assert_eq!(box_state("codex", &s), BoxState::NonEmpty);
    }

    /// 同一個框，真的有人打了字（或多打一格）就不是空的。
    #[test]
    fn a_real_composer_with_typed_text_is_not_empty() {
        use crate::lifecycle::{box_state, BoxState};
        let grok_typed = GROK_SCREEN.replacen("│ ❯                                                                        │", "│ ❯ 我在打字                                                               │", 1);
        assert_eq!(box_state("grok", &grok_typed), BoxState::NonEmpty);
        let codex_typed = CODEX_IDLE_SPLASH.replacen("› Ask Codex to do anything", "› Ask Codex to do anything please", 1);
        assert_eq!(box_state("codex", &codex_typed), BoxState::NonEmpty, "佔位字多一個字就是使用者的字");
        let codex_space = CODEX_IDLE_SPLASH.replacen("› Ask Codex to do anything", "›  ", 1);
        assert_eq!(box_state("codex", &codex_space), BoxState::NonEmpty, "marker 後多打一格");
    }

    #[test]
    fn grok_reply_comes_from_clean_screen() {
        assert_eq!(extract_reply("grok", GROK_SCREEN), None);
        assert_eq!(clean_screen("grok", GROK_SCREEN).unwrap(), "GROK-OK");
    }

    /// Narrower pane: the banner wraps differently and the header carries a git branch.
    const GROK_SCREEN_NARROW: &str = "\
   main ~/project/agents-manager                                15K / 500K
     ❯ Reply with exactly GROK-FALLBACK                           2:20 AM
                                                                            █
     ◆ user_prompt_submit  [hooks: 1]                                       █
     ◆ Thought for 0.3s                                                     █
     GROK-FALLBACK                                                2:20 AM   █
     Worked for 3.3s                                     stop  [hooks: 2]   █
  Help improve Grok                                      [Opt out] [Opt in]
  Off by default. Opt-in to allow SpaceXAI to retain coding data, e.g.,
  prompts, traces, & metrics, for training and debugging purposes.
  Change anytime via settings.
  Read Terms and Privacy Policy.
  ╭───────────────────────────────────────────────────────────────────────╮
  │ ❯                                                                     │
  ╰──────────────────────────────────── Grok 4.5 (high) · always-approve ─╯
  Shift+Tab:mode  │  Ctrl+.:shortcuts
";

    /// grok echo of an attachment prompt (`01M1XSVME9SKEG1NZXG51HFP73`, 2026-09-07): lines 2..n
    /// squeezed onto one `…` row, which was stored as the answer.
    const GROK_ATTACHMENT_ECHO: &str = "\

   main ~/project/agents-manager                                          250K / 500K


     ❯ 是否能有更好的ui表示法                                                7:25 PM

       附加圖片（請讀取這個檔案來查看）： …


     ◆ user_prompt_submit  [hooks: 1]

                                                                                       █

    ⠴ Waiting for response… 16s                                       16s ⇣250k [stop]

  Help improve Grok                                                 [Opt out] [Opt in]
  Off by default. Opt-in to allow SpaceXAI to retain coding data,
  e.g., prompts, traces, & metrics, for training and debugging
  purposes. Change anytime via settings.
  Read Terms and Privacy Policy.

  ╭──────────────────────────────────────────────────────────────────────────────────╮
  │ ❯                                                                                │
  ╰─────────────────────────────────────────────── Grok 4.6 (high) · always-approve ─╯

  Shift+Tab:mode  │  Esc:cancel  │  Ctrl+.:shortcuts
";

    /// What the agent was handed: typed text plus `attach::deliver_text`'s block.
    const SENT_WITH_ATTACHMENT: &str = "是否能有更好的ui表示法\n\n附加圖片（請讀取這個檔案來查看）：\n/Users/m1pro/project/agents-manager/.agents-manager/attachments/01M1XSTPGPMTZ3HYENTBP36125-2026-09-07---7-25-01.png";

    /// Reads as an answer until the echo is removed; the clipped row is all that remains.
    #[test]
    fn a_clipped_attachment_echo_is_not_an_answer() {
        assert_eq!(clean_screen("grok", GROK_ATTACHMENT_ECHO).unwrap(), "附加圖片（請讀取這個檔案來查看）： …");
        let sent = vec![SENT_WITH_ATTACHMENT.to_string()];
        assert_eq!(screen_reply("grok", GROK_ATTACHMENT_ECHO, &sent), None);
    }

    /// Only the delivered text carries the attachment block; matching the typed text let the echo through.
    #[test]
    fn the_typed_line_alone_does_not_cover_the_echo() {
        let typed = vec!["是否能有更好的ui表示法".to_string()];
        assert!(screen_reply("grok", GROK_ATTACHMENT_ECHO, &typed).is_some());
    }

    /// A real reply after the clipped echo survives — only the echo is taken off.
    #[test]
    fn a_reply_after_a_clipped_echo_survives() {
        let screen = GROK_ATTACHMENT_ECHO.replace(
            "附加圖片（請讀取這個檔案來查看）： …",
            "附加圖片（請讀取這個檔案來查看）： …\n\n     好的，我看過圖了。",
        );
        let sent = vec![SENT_WITH_ATTACHMENT.to_string()];
        assert_eq!(screen_reply("grok", &screen, &sent).unwrap(), "好的，我看過圖了。");
    }

    /// The clipped-echo rule keys on content, not `…`: a reply that trails off survives.
    #[test]
    fn a_reply_that_merely_ends_in_an_ellipsis_is_kept() {
        let text = "不太確定，讓我先看看那個檔案…";
        assert_eq!(strip_echoed_prompt(text, SENT_WITH_ATTACHMENT), text);
    }

    /// ...and neither is a fragment too short to be sure about.
    #[test]
    fn a_short_clipped_line_is_not_treated_as_an_echo() {
        assert!(!is_clipped_echo("附加…", &["附加圖片（請讀取這個檔案來查看）："]));
        assert!(is_clipped_echo("附加圖片（請讀取這個檔案來查看）： …", &["附加圖片（請讀取這個檔案來查看）：", "/tmp/a.png"]));
    }

    // composer check behind the stall watchdog's Enter nudge

    /// claude 2.1.263 compacting a 673k session (2026-09-07 11:21): Enter swallowed, text left in the box.
    const CLAUDE_UNSENT_PROMPT: &str = "\
❯ 直接做，且要確保claude裝有herdr 的skill

  Ran 3 shell commands

⏺ 已派出 agents-manager-6verqr-track（pane w8:p25），正在讀 config / state 開始做。

✻ Worked for 2m 14s · done 3:34 PM
                                                new task? /clear to save 673k tokens
──────────────────────────────────────────────────────────────────────────────────
❯ [Image #6]試著對claude max方案增加 fable用量的讀取
  附加圖片（請讀取這個檔案來查看）：
──────────────────────────────────────────────────────────────────────────────────
  tony. | agents-manager | Fable 5.1 67% | 5h:- | 7d:92%(rst 6d 16h) | F5:85%   /rc
  ⏵⏵ bypass permissions on (shift+tab to cycle)
";

    const SENT_UNSENT_PROMPT: &str = "試著對claude max方案增加 fable用量的讀取\n\n附加圖片（請讀取這個檔案來查看）：\n/Users/m1pro/project/agents-manager/.agents-manager/attachments/x.png";

    #[test]
    fn an_unsent_prompt_is_seen_in_the_composer() {
        // The box, not the transcript echo; `[Image #6]` in front doesn't hide it.
        assert_eq!(
            composer_text("claude", CLAUDE_UNSENT_PROMPT).unwrap(),
            "[Image #6]試著對claude max方案增加 fable用量的讀取\n附加圖片（請讀取這個檔案來查看）："
        );
        assert!(composer_holds_prompt("claude", CLAUDE_UNSENT_PROMPT, SENT_UNSENT_PROMPT));
    }

    /// claude draws U+00A0 after `❯` (pane read 2026-09-07); the marker must accept any blank.
    #[test]
    fn a_no_break_space_after_the_marker_still_reads_as_the_composer() {
        let screen = CLAUDE_UNSENT_PROMPT.replace("❯ [Image #6]", "❯\u{a0}[Image #6]");
        assert!(composer_text("claude", &screen).is_some());
        assert!(composer_holds_prompt("claude", &screen, SENT_UNSENT_PROMPT));
        assert!(!pane_awaits_input("claude", &screen));
    }

    /// Prompt taken: box empty, echo only in the transcript. Enter here would submit an empty prompt.
    #[test]
    fn an_accepted_prompt_leaves_the_composer_empty() {
        let screen = CLAUDE_UNSENT_PROMPT.replace(
            "❯ [Image #6]試著對claude max方案增加 fable用量的讀取\n  附加圖片（請讀取這個檔案來查看）：",
            "❯",
        );
        assert_eq!(composer_text("claude", &screen), None);
        assert!(!composer_holds_prompt("claude", &screen, SENT_UNSENT_PROMPT));
    }

    /// Someone else's text in the box is not ours.
    #[test]
    fn other_text_in_the_composer_is_not_our_prompt() {
        assert!(!composer_holds_prompt("claude", CLAUDE_UNSENT_PROMPT, "完全不一樣的另一個問題"));
        // grok's empty box (`│ ❯ │`) reads as empty through its border glyphs too.
        assert_eq!(composer_text("grok", GROK_SCREEN), None);
    }

    /// claude 把長段貼上摺成 `[Pasted text #N +M lines]`（2026-09-19 AM-1-XH 沒收到 v4 的 blocked 通知）：
    /// 行數對得上就是我們送的那則、該補 Enter；對不上（使用者自己貼的）不是。
    #[test]
    fn a_folded_paste_whose_line_count_matches_is_our_prompt() {
        let sent = "[daemon 自動通知]子 agent v4 停在 blocked\n\n原文：\n```text\nDo you want to proceed?\n```\n要回它就用 herdr agent prompt";
        assert_eq!(sent.matches('\n').count(), 6);
        let folded = CLAUDE_UNSENT_PROMPT.replace(
            "❯ [Image #6]試著對claude max方案增加 fable用量的讀取\n  附加圖片（請讀取這個檔案來查看）：",
            "❯ [Pasted text #3 +6 lines]",
        );
        assert!(composer_holds_prompt("claude", &folded, sent));
        // 行數不同：不是我們的貼上，不按 Enter。
        assert!(!composer_holds_prompt("claude", &folded, "第一行\n第二行"));
        // 摺起來的貼上後面還有別的字：使用者在打字，不動它。
        let typing = folded.replace("+6 lines]", "+6 lines] 再補一句");
        assert!(!composer_holds_prompt("claude", &typing, sent));
        // 只有 claude 會摺。
        assert!(!composer_holds_prompt("codex", &folded, sent));
    }

    /// A prompt too short to identify is never matched — every screen contains "hi".
    #[test]
    fn a_very_short_prompt_is_not_matched() {
        assert!(!composer_holds_prompt("claude", CLAUDE_UNSENT_PROMPT, "試"));
    }

    #[test]
    fn the_stall_message_says_an_enter_was_re_sent() {
        assert!(stall_reason(&[], true).contains("補送"));
        assert!(!stall_reason(&[], false).contains("補送"));
    }

    #[test]
    fn grok_banner_is_skipped_as_a_block() {
        assert_eq!(clean_screen("grok", GROK_SCREEN_NARROW).unwrap(), "GROK-FALLBACK");
    }

    #[test]
    fn a_mid_tool_call_screen_is_not_an_answer() {
        // The exact shape that closed a turn with the wrong content (2026-09-07).
        assert!(is_tool_progress("Running 1 shell command…\n\nTip: Run /install-slack-app to use it"));
        assert!(is_tool_progress("Running 4 shell commands…"));
        assert!(is_tool_progress("  Running 1 shell command…  \n  esc to interrupt  "));
    }

    #[test]
    fn a_turning_spinner_means_the_turn_is_not_over() {
        // The screen that closed a turn with chrome as its answer (2026-09-07 11:17).
        let screen = "⏺ 上一句回覆\n\n✢ Baking…\n  ⎿  Tip: Use /memory to view and manage Claude memory\n✗ Auto-update failed · Run claude doctor\n❯ ";
        assert!(pane_still_busy(screen));
        assert!(pane_still_busy("· Philosophising… (33m 33s · ↓ 94.9k tokens)"));
        assert!(pane_still_busy("⠦ Thinking… 52s"));
        // Finished-spinner lines are not "busy".
        assert!(!pane_still_busy("✻ Crunched for 9s · done 11:35 PM\n❯ "));
        assert!(!pane_still_busy("✻ Baked for 25m 50s · done 11:01 AM"));
        assert!(!pane_still_busy("⏺ 做完了。\n❯ "));
        // And none of that chrome survives into a stored reply.
        let screen = "❯ hi\n✢ Baking…\nTip: Use /memory\n✗ Auto-update failed · Run claude doctor\n⏺ 真正的回覆\n╭───╮\n│ ❯ │\n╰───╯";
        assert_eq!(extract_reply("claude", screen).unwrap(), "真正的回覆");
        assert_eq!(clean_screen("claude", screen).unwrap(), "⏺ 真正的回覆");
        // Chrome alone is nothing at all — never a stored "reply".
        assert!(clean_screen("claude", "❯ hi\n✢ Baking…\nTip: Use /memory\n✗ Auto-update failed · Run claude doctor\n❯ ").is_none());
    }

    /// 2026-09-19 AM-1-XH-2：回合結束後版本列還在輸入框上方，終端備援把它接在回覆最後一行。
    #[test]
    fn the_update_banner_above_the_composer_is_not_part_of_the_reply() {
        let screen = "❯ 看一下孫代\n⏺ parent_bot_id 正確指向 is102103。\n  要不要我開一張 UI issue。\n\n✻ Worked for 15s · done 12:49 · 1 shell still running\n                                   current: 2.1.276 · latest: 2.1.277 ✔ Update installed · Restart to update\n──────\n❯\n──────\n  xavie | agents-manager | OP5 H 47% | 5h:94%\n";
        assert_eq!(extract_reply("claude", screen).unwrap(), "parent_bot_id 正確指向 is102103。\n  要不要我開一張 UI issue。");
        // 還沒裝好時只有前半段；裝好後被擠到只剩後半段也一樣。
        for banner in ["current: 2.1.276 · latest: 2.1.277", "✔ Update installed · Restart to update"] {
            let s = screen.replace("current: 2.1.276 · latest: 2.1.277 ✔ Update installed · Restart to update", banner);
            assert!(!extract_reply("claude", &s).unwrap().contains("2.1.27"), "{banner}");
            assert!(!extract_reply("claude", &s).unwrap().contains("Restart"), "{banner}");
        }
        // 回覆裡**講到**這句話的照留。
        let talk = "❯ hi\n⏺ 狀態列會印 current: 2.1.276 · latest: 2.1.277 ✔ Update installed · Restart to update，要重啟才套用。\n╭───╮\n│ ❯ │\n╰───╯";
        assert!(extract_reply("claude", talk).unwrap().contains("要重啟才套用"));
    }

    #[test]
    fn a_real_reply_is_still_stored() {
        // Finished tools (`Ran`, no ellipsis) come with the answer; do not throw that away.
        assert!(!is_tool_progress("Ran 4 shell commands\n\n已追加給同一個 agent 一起做。"));
        // A reply that merely talks about running commands is a reply.
        assert!(!is_tool_progress("我會 Running 1 shell command… 之後再回報結果"));
        // A lone Tip line is a different symptom and must not be swallowed here.
        assert!(!is_tool_progress("Tip: Run /ultrareview for a cloud-based review"));
        assert!(!is_tool_progress(""));
    }

    /// claude mid-turn, nothing printed yet: all chrome, but the user still needs to see it thinking.
    const THINKING_ONLY: &str = "\
❯ 幫我看一下這個 bug
✻ Thinking… (12s · ↑ 1.2k tokens · esc to interrupt)
╭────────────────────────────────────────────╮
│ ❯                                          │
╰────────────────────────────────────────────╯
  tony. | pt | HAI4.5 | 5h:- | 7d:-
  ⏵⏵ bypass permissions on (shift+tab to cycle)
";



    /// 2026-09-06: lines 2..n of a multi-line prompt echo were stored as the agent's answer.
    const ECHOED_BACK: &str = "\
❯ 併行
1 沒事 bot 不會需要停止的動作
2 執行中 能夠show session name agent取的名字

✛ Generating… (4s · thinking)
";
    const SENT: &str = "併行\n1 沒事 bot 不會需要停止的動作\n2 執行中 能夠show session name agent取的名字";

    #[test]
    fn the_users_own_prompt_does_not_come_back_as_the_reply() {
        let scraped = clean_screen("claude", ECHOED_BACK).unwrap_or_default();
        assert_eq!(strip_echoed_prompt(&scraped, SENT), "");
    }

    #[test]
    fn an_unknown_spinner_glyph_never_reaches_a_message() {
        // `✛` is in no glyph list, so only `is_activity_shape` keeps it out of `clean_screen`.
        assert_eq!(clean_screen("claude", "❯ go\n✛ Generating… (4s · thinking)\n"), None);
    }

    #[test]
    fn strip_echoed_prompt_keeps_a_real_reply() {
        let text = "1 沒事 bot 不會需要停止的動作\n2 執行中 能夠show session name agent取的名字\n好的，我來處理。";
        assert_eq!(strip_echoed_prompt(text, SENT), "好的，我來處理。");
    }

    /// 2026-09-06: a pane a couple of columns wide echoed the prompt one glyph per line; it was stored as the reply.
    #[test]
    fn a_prompt_shredded_one_glyph_per_line_is_still_recognised_as_the_echo() {
        let sent = "不要依剩餘量重排 固定 cc0 cc1 codex grok";
        let shredded = "要\n依\n剩\n餘\n量\n重\n排\n固\n定\nc\nc\n0\nc\nc\n1\nc\no\nd\ne\nx\ng\nr\no\nk";
        assert_eq!(strip_echoed_prompt(shredded, sent), "");
        // …and with a real answer after it, only the echo goes.
        let with_reply = format!("{shredded}\n好\n的");
        assert_eq!(strip_echoed_prompt(&with_reply, sent), "好\n的");
    }

    #[test]
    fn the_squashed_strip_does_not_eat_a_reply_that_merely_starts_alike() {
        // Shares only three characters with the prompt: nowhere near SQUASH_MIN.
        assert_eq!(strip_echoed_prompt("不要這樣做，我改用別的方法。", SENT), "不要這樣做，我改用別的方法。");
        // No prompt at all to match against.
        assert_eq!(strip_echoed_prompt("PONG", "hi"), "PONG");
    }

    #[test]
    fn shredded_output_is_recognised() {
        assert!(is_shredded("要\n依\n剩\n餘\n量\n重\n排\n固\n定"));
        // A normal reply is not shredded, however many short lines it happens to have.
        assert!(!is_shredded("好的，我來處理。\n第一步：讀設定。\n第二步：改程式。"));
        // Too few lines to judge.
        assert!(!is_shredded("要\n依\n剩"));
    }

    #[test]
    fn strip_echoed_prompt_leaves_a_partial_match_alone() {
        // The pane wrapped line 2 away: dropping half a real reply is worse than a duplicate.
        let text = "1 沒事 bot 不會需要停止的動作\n好的，我來處理。";
        assert_eq!(strip_echoed_prompt(text, SENT), text);
        // A single-line prompt has no tail to strip.
        assert_eq!(strip_echoed_prompt("PONG", "ping"), "PONG");
    }

    /// grok pane `w8:pK` (2026-09-06): finished at an empty boxed composer while herdr said `working`.
    const GROK_AWAITING: &str = "\
     一
     次
     。
             █

  Help impro
  Off by
  default.

  ╭────────╮
  │ ❯      │
  ╰─ Grok ─╯

  Shift+Tab:
";

    /// `w8:pK` (2026-09-06): every CJK char on its own row; `strip_echoed_prompt` rightly refuses
    /// partial matches, so shredding must be detected.
    #[test]
    fn a_pane_too_narrow_to_read_is_recognised() {
        let shredded = "要\n依\n剩\n餘\n量\n重\n排\n固\n定\nc\nc\n0\nHelp impro\n";
        assert!(is_shredded(shredded));
        // A normal reply must never be mistaken for one, however short its lines are.
        assert!(!is_shredded("好的，我來處理。\n改了三個檔案：\n- a.rs\n- b.rs\n- c.rs\n都跑過測試了。\n"));
        // Too little to judge: a two-line answer is not evidence of a broken pane.
        assert!(!is_shredded("好\n的\n"));

        // Verbatim `w8:pK` (2026-09-06): short-line count alone let it through; widest row = 4 chars gives it away.
        let real = "381K\n❯\n█\n█\n▼\nHel\nOff\nby\ndef\nau…\n";
        assert!(is_shredded(real));

        // The width rule must not fire on a narrow *but legible* reply.
        assert!(!is_shredded("已修好。\n改了 db.rs。\n測試全過。\n沒有其他影響。\n重啟後生效。\n請確認。\n"));
    }

    #[test]
    fn an_empty_composer_is_recognised_through_the_box_frame() {
        assert!(pane_awaits_input("grok", GROK_AWAITING));
        assert!(pane_awaits_input("claude", "⏺ done\n╭────╮\n│ ❯  │\n╰────╯\n"));
        assert!(pane_awaits_input("codex", "• done\n╭────╮\n│ ›  │\n╰────╯\n"));
        // True while claude works too — why the caller pairs it with "nothing changed for N polls".
        assert!(pane_awaits_input("claude", THINKING_ONLY));
    }

    #[test]
    fn a_composer_with_text_in_it_is_not_awaiting_input() {
        assert!(!pane_awaits_input("grok", "╭────────╮\n│ ❯ hi   │\n╰─ Grok ─╯\n"));
        assert!(!pane_awaits_input("claude", "⏺ still writing the answer\n"));
        // Only the tail is searched: an old empty prompt scrolled far up must not count.
        let mut s = String::from("╭──╮\n│ ❯ │\n╰──╯\n");
        for _ in 0..20 {
            s.push_str("output line\n");
        }
        assert!(!pane_awaits_input("claude", &s));
    }

    #[test]
    fn live_activity_surfaces_the_spinner_when_there_is_no_text() {
        assert_eq!(live_reply("claude", THINKING_ONLY), None);
        assert_eq!(live_activity("claude", THINKING_ONLY).unwrap(), "Thinking… (12s · ↑ 1.2k tokens · esc to interrupt)");
    }

    /// Once text prints, `live_reply` works as before and the activity row is still reported.
    #[test]
    fn live_reply_still_wins_once_there_is_output() {
        let screen = "❯ Reply with PONG\n⏺ PONG\n✻ Cooked for 5s\n──────\n❯\n";
        assert_eq!(live_reply("claude", screen).unwrap(), "PONG");
        assert_eq!(live_activity("claude", screen).unwrap(), "Cooked for 5s");
    }

    #[test]
    fn live_activity_takes_the_last_row_and_is_capped() {
        let screen = format!("❯ go\n✻ Thinking…\n✻ {}\n", "x".repeat(200));
        let got = live_activity("claude", &screen).unwrap();
        assert_eq!(got.chars().count(), ACTIVITY_MAX + 1);
        assert!(got.ends_with('…'));
    }

    /// grok `◆` rows carry the scrollbar glyph: `strip_grok_decor` first.
    #[test]
    fn live_activity_reads_grok_event_rows() {
        assert_eq!(live_activity("grok", GROK_SCREEN).unwrap(), "Thought for 0.1s");
    }

    /// Real capture: three minutes in, only this row, UI still 「等待回覆（hook）…」.
    const BOOGIEING: &str = "\
❯ 幫我重構一下
✻ Boogieing… (3m 18s · ↓ 11.0k tokens)
╭────────────────────────────────────────────╮
│ ❯                                          │
╰────────────────────────────────────────────╯
  tony. | pt | HAI4.5 | 5h:- | 7d:-
";

    #[test]
    fn live_activity_reports_the_real_stuck_frame() {
        assert_eq!(live_reply("claude", BOOGIEING), None);
        assert_eq!(live_activity("claude", BOOGIEING).unwrap(), "Boogieing… (3m 18s · ↓ 11.0k tokens)");
    }

    /// Unknown glyph, or none, still recognised by shape.
    #[test]
    fn live_activity_falls_back_to_shape_for_unknown_glyphs() {
        let unknown = "❯ go\n⣾ Puttering… (12s · ↑ 1.2k tokens)\n";
        assert_eq!(live_activity("claude", unknown).unwrap(), "Puttering… (12s · ↑ 1.2k tokens)");
        let bare = "❯ go\nSimmering… (1m 4s)\n";
        assert_eq!(live_activity("claude", bare).unwrap(), "Simmering… (1m 4s)");
    }

    /// …but the shape test must not swallow ordinary prose that happens to use an ellipsis.
    #[test]
    fn activity_shape_ignores_prose() {
        assert!(!is_activity_shape("等一下… (我先看看)"));
        assert!(!is_activity_shape("好的… (see the note below)"));
        assert!(is_activity_shape("Boogieing… (3m 18s · ↓ 11.0k tokens)"));
        assert!(is_activity_shape("✢ Improvising… (5s)"));
    }

    /// Before the prompt echo nothing is reported (no previous turn's spinner).
    #[test]
    fn live_activity_ignores_the_previous_turn() {
        let screen = "❯ echo 1\n✻ Worked for 9s\n❯ echo 2\n";
        assert_eq!(live_activity("claude", screen), None);
    }

    #[test]
    fn grok_decor_strip_keeps_prose_times() {
        assert_eq!(strip_grok_decor("     GROK-OK                 2:09 AM   █"), "     GROK-OK");
        assert_eq!(strip_grok_decor("meet at 2:09 PM"), "meet at 2:09 PM");
        assert_eq!(strip_grok_decor("plain line █"), "plain line");
    }

    /// CLI-typed prompt read off the pane echo; the last echo wins.
    #[test]
    fn last_prompt_echo_text_reads_what_the_user_typed() {
        assert_eq!(last_prompt_echo_text("claude", TWO_TURNS).as_deref(), Some("echo 2"));
        assert_eq!(last_prompt_echo_text("claude", NOT_LOGGED_IN).as_deref(), Some("echo 1"));
        assert_eq!(last_prompt_echo_text("codex", "› 幫我看一下這個 bug\n  thinking…\n").as_deref(), Some("幫我看一下這個 bug"));
    }

    /// grok 常駐的 telemetry 橫幅不算「畫面有東西」，否則 [`idle_threshold`] 誤判講完了（2026-09-13 GROK）。
    #[test]
    fn the_grok_opt_in_banner_is_not_content() {
        let screen = "❯ fix ui\n\n  Help improve Grok                                    [Opt out] [Opt in]\n  Off by default. Opt-in to allow SpaceXAI to retain coding data, e.g.,\n  prompts, traces, & metrics, for training and debugging purposes.\n  Change anytime via settings.\n  Read Terms and Privacy Policy.\n\n  ╭──────────────────────────────╮\n  │ ❯                            │\n  ╰──────── Grok 4.6 (low) ──────╯\n\n  Shift+Tab:mode  │  Ctrl+.:shortcuts\n";
        assert!(live_reply("grok", screen).unwrap_or_default().trim().is_empty(), "橫幅不是回覆");
        assert!(pane_awaits_input("grok", screen));
    }

    /// 2026-09-13（GROK／w168:pN）：15 秒還是空 `❯` 被當成等輸入，回覆 36 秒後才到。沒印過東西要多等。
    #[test]
    fn a_pane_that_never_rendered_anything_gets_a_longer_grace() {
        // 只有「herdr 說閒著」+「印過東西然後停住」才走短的那條。
        assert_eq!(idle_threshold(true, "idle"), IDLE_POLLS);
        assert_eq!(idle_threshold(false, "idle"), IDLE_POLLS_SILENT);
        // herdr 說還在跑：可能真的在做事（2026-09-13 GROK）。
        assert_eq!(idle_threshold(true, "working"), IDLE_POLLS_SILENT);
        assert_eq!(idle_threshold(false, "working"), IDLE_POLLS_SILENT);
        // 讀不到狀態時不要比原本更急。
        assert_eq!(idle_threshold(true, ""), IDLE_POLLS);
        assert!(IDLE_POLLS_SILENT > IDLE_POLLS * 3, "要明顯長過那 14 秒，不然等於沒改");
    }

    /// 2026-09-12 使用者實機：長 prompt 折成兩行，下半句被當成回覆。續行要算進回音。
    #[test]
    fn last_prompt_echo_text_takes_the_wrapped_continuation() {
        let screen = "❯ 請直接呼叫 AskUserQuestion 工具問我兩題：第二題 header『功能』請設 multiSelect:\n  true，四個選項：『站內搜尋』『SEO 是主要目的』。問完就停著等我回答。\n  ⎿  You've reached your Fable limit. Run /usage-credits to continue.\n\n✻ Worked for 0s · done 5:08 PM\n";
        let got = last_prompt_echo_text("claude", screen).expect("有回音");
        assert!(got.starts_with("請直接呼叫 AskUserQuestion"), "第一行還在：{got}");
        assert!(got.contains("問完就停著等我回答。"), "折行的下半句要收進來：{got}");
        // 工具結果那行不是使用者說的話。
        assert!(!got.contains("Fable limit"), "`⎿` 開頭的是 claude 的輸出：{got}");
    }

    /// 續行只吃「縮排且不是別的東西」，寧可保守；回音沒排到行尾就沒有續行。
    #[test]
    fn a_short_echo_row_has_no_continuation() {
        assert!(!echo_row_is_full("❯ echo 2"));
        assert!(!echo_row_is_full("› 幫我看一下這個 bug"));
        assert!(echo_row_is_full("❯ 請直接呼叫 AskUserQuestion 工具問我兩題：第二題 header『功能』請設 multiSelect:"));
    }

    #[test]
    fn echo_continuation_stops_at_anything_that_is_not_the_same_sentence() {
        // 沒縮排 = 下一塊內容
        assert_eq!(echo_continuation("claude", "⏺ 我看了一下"), None);
        assert_eq!(echo_continuation("claude", "done"), None);
        // 縮排但是輸出標記／狀態列／框線
        for l in ["  ⎿  結果", "  ⏺ 回覆", "  ✻ Worked for 0s", "  │ box", "  ╭─────"] {
            assert_eq!(echo_continuation("claude", l), None, "{l} 不是續行");
        }
        // 空行
        assert_eq!(echo_continuation("claude", "   "), None);
        // 下一個回音（使用者連送兩句）
        assert_eq!(echo_continuation("claude", "  ❯ 第二句"), None);
        // 真的續行
        assert_eq!(echo_continuation("claude", "  第二半句"), Some("第二半句"));
    }

    /// 收進續行後 `strip_echoed_prompt` 才吃得到整段回音——症狀真正修掉的地方。
    #[test]
    fn the_wrapped_half_no_longer_looks_like_a_reply() {
        let screen = "❯ 第一半句很長很長，長到排滿整行才會折到下一行去，這是折行的前提\n  第二半句也不短\n  ⎿  真正的回覆\n";
        let prompt = last_prompt_echo_text("claude", screen).expect("有回音");
        let left = strip_echoed_prompt("第二半句也不短\n⎿  真正的回覆", &prompt);
        assert!(!left.contains("第二半句"), "回音要被剝掉：{left}");
        assert!(left.contains("真正的回覆"), "回覆要留著：{left}");
    }

    /// grok's clock and scrollbar glyph on the echo row are not part of the prompt.
    #[test]
    fn last_prompt_echo_text_strips_grok_decor() {
        let screen = "❯ Reply with GROK-OK                    2:09 AM   █\n     GROK-OK\n";
        assert_eq!(last_prompt_echo_text("grok", screen).as_deref(), Some("Reply with GROK-OK"));
    }

    /// No echo / empty box / unknown CLI: report nothing (`begin_external_turn` opens with no user message).
    #[test]
    fn last_prompt_echo_text_is_none_without_an_echo() {
        assert_eq!(last_prompt_echo_text("claude", "⏺ orphaned reply\n"), None);
        assert_eq!(last_prompt_echo_text("claude", "❯\n"), None);
        assert_eq!(last_prompt_echo_text("claude", "❯    \n"), None);
        assert_eq!(last_prompt_echo_text("unknown", "❯ hello\n"), None);
    }
}


/// #207：codex 0.155 的畫面。**不是實抓**——照 codex rust-v0.155.0 的原始碼與它自己的 snapshot 組出來的（完成行：
/// `tui/src/history_cell/separators.rs`、`chatwidget/snapshots/*completion_after_plain_answer.snap`；狀態列：
/// `tui/src/status_indicator_widget.rs`）。升級之前照 SPEC §4.3「codex 0.155」的步驟實抓，換成 `fixtures/codex-0.155-*.txt`。
#[cfg(test)]
mod codex_0155_screen_tests {
    use super::*;

    const FOOTER: &str = "  gpt-6-astra low · ~/project/agents-manager · Context 3% used · 5h 82% left · weekly 97% left";

    /// 回合剛成功結束：回覆、空一行、完成時間、空一行、空框、狀態列。
    fn finished(completion: &str) -> String {
        format!("› Reply with PONG\n\n• PONG\n\n  {completion}\n\n› Ask Codex to do anything\n\n{FOOTER}\n")
    }

    /// 回合中：狀態列的字頭是 `header`（預設 `Working`，summary 開著時是 summary 的最新一行）。
    fn thinking(header: &str) -> String {
        format!("› Reply with PONG\n\n• {header} (12s • esc to interrupt)\n\n› Ask Codex to do anything\n\n{FOOTER}\n")
    }

    const COMPLETIONS: [&str; 6] = [
        "done 3:24 PM",
        "Worked for 2m 5s · done 3:24 PM",
        "done Sep 6 at 2:32 PM",
        "Worked for 1h 2m 3s · done Sep 6, 2000 at 2:32 PM",
        "Worked for 2m 5s",
        "Worked for 2m 5s · done 12:05 AM · Local tools: 2 calls (1.2s)",
    ];

    #[test]
    fn the_completion_line_is_recognised_and_nothing_else_is() {
        for c in COMPLETIONS {
            assert!(is_codex_completion_line(c), "{c}");
            assert!(is_codex_completion_line(&format!("  {c}  ")), "縮排：{c}");
        }
        for not in [
            "done",
            "done soon",
            "done 3:24",
            "done 13:24 PM",
            "done 3:4 PM",
            "Done 3:24 PM",
            "done Sept 6 at 2:32 PM",
            "done Sep 6 2000 at 2:32 PM",
            "Worked for the team · done 3:24 PM",
            "Worked for 2 minutes",
            "• done 3:24 PM",
            "The job was done 3:24 PM",
        ] {
            assert!(!is_codex_completion_line(not), "{not}");
        }
    }

    /// 回合剛結束的畫面：切出來的回覆不帶時間戳（extract_reply 與沒有標記時的 clean_screen 都是）。
    #[test]
    fn the_completion_line_is_not_part_of_the_reply() {
        for c in COMPLETIONS {
            let screen = finished(c);
            assert_eq!(extract_reply("codex", &screen).as_deref(), Some("PONG"), "{c}");
            let cleaned = clean_screen("codex", &screen).unwrap();
            assert!(!cleaned.contains("done") && !cleaned.contains("Worked for"), "{c} → {cleaned:?}");
            assert!(cleaned.contains("PONG"));
        }
    }

    /// 只剝回覆**尾巴**那一行：回覆中間剛好有一行長得一樣的字（codex 自己的測試就有）照留。
    #[test]
    fn a_reply_line_that_only_looks_like_a_completion_line_is_kept() {
        let screen = format!("› when?\n\n• It finished:\n  done 3:24 PM\n  and then it stopped.\n\n  done 3:25 PM\n\n› Ask Codex to do anything\n\n{FOOTER}\n");
        assert_eq!(extract_reply("codex", &screen).as_deref(), Some("It finished:\n  done 3:24 PM\n  and then it stopped."));
    }

    /// 字頭換成 reasoning summary、壓縮中、狀態列後面接訊息、中斷鍵改綁，都還是忙；結束的畫面、工具結果不是。
    #[test]
    fn a_status_row_is_busy_whatever_its_header_says() {
        for header in ["Working", "Planning the fix for the poller", "Compacting context", "Reading `screen.rs` (again)"] {
            assert!(pane_still_busy(&thinking(header)), "{header}");
        }
        assert!(pane_still_busy("• Reviewing the diff (1m 05s • esc to interrupt) · 2 background terminals running\n"));
        assert!(pane_still_busy("• Reviewing (1h 02m 03s • ctrl + c to interrupt)\n"), "中斷鍵改綁");
        for c in COMPLETIONS {
            assert!(!pane_still_busy(&finished(c)), "回合結束的畫面不是忙：{c}");
        }
        assert!(!pane_still_busy("• Ran cargo test (4 passed)\n"), "工具結果不是狀態列");
        assert!(!pane_still_busy("• The fix (see above • done) is in.\n"));
    }

    /// 狀態列的模型／強度／context 與額度照讀（summary 裡剛好有 `Context`、`5h … left` 也不會蓋掉最底下那一行）；
    /// 框還是在同一個地方。
    #[test]
    fn the_status_line_and_the_composer_are_read_as_before() {
        let noisy = thinking("Checking Context usage against 5h 10% left · weekly 1% left");
        for screen in [finished("Worked for 2m 5s · done 3:24 PM"), thinking("Planning the fix"), noisy] {
            let rt = crate::codex_live::parse_status_line(&screen).expect("讀得到狀態列");
            assert_eq!((rt.model.as_str(), rt.effort.as_deref()), ("gpt-6-astra", Some("low")), "{screen}");
            let q = crate::codex_live::parse_status_quota(&screen).expect("讀得到額度");
            assert_eq!((q.five_hour_left, q.weekly_left), (Some(82.0), Some(97.0)), "{screen}");
            let styled = screen.replace("› Ask Codex to do anything", "› \u{1b}[2mAsk Codex to do anything\u{1b}[22m");
            assert_eq!(crate::lifecycle::box_state("codex", &styled), crate::lifecycle::BoxState::Empty, "{screen}");
        }
    }
}

/// 撞限橫幅寫進額度那一格（`apply_codex_limit_hit_quota`）的去重規則。
#[cfg(test)]
mod limit_hit_quota_tests {
    use super::*;
    use crate::testing as tt;

    const NOTICE: &str = "ERROR: You've hit your usage limit, or try again at 10:15 PM.";

    async fn apply(app: &Arc<App>) {
        apply_codex_limit_hit_quota(app, LOCAL_HOST, "codex", codex_limit_hit(NOTICE, crate::db::now())).await;
    }

    /// review3 c2 M1：`until` 到了、交辦重送、CLI 回同一句橫幅——這是真的又被擋一次，不是重掃舊字。
    /// 以前同文就直接略過，記憶體裡只剩那張過期的，`on_turn_done` 查不到撞限，交辦被結成 failed 送去驗收。
    #[tokio::test]
    async fn the_same_banner_after_the_old_one_expired_is_a_fresh_hit() {
        let env = tt::env().await;
        let app = env.app.clone();
        apply(&app).await;
        // 改成已經過期，模擬「等到重置時間、重送一次，又撞到同一句」。
        {
            let mut q = app.quotas.lock().await;
            let hit = q.get_mut("codex").unwrap().limit_hit.as_mut().unwrap();
            hit.until = Some("2020-01-01T00:00:00Z".into());
            hit.at = "2020-01-01T00:00:00Z".into();
        }

        apply(&app).await;

        let hit = app.quotas.lock().await.get("codex").unwrap().limit_hit.clone().expect("又被擋一次要重新記上");
        assert!(!crate::quota::limit_hit_expired(Some(&hit)), "新的那張不該是過期的：{hit:?}");
        assert_ne!(hit.at, "2020-01-01T00:00:00Z", "時間戳要更新成這一次");
    }

    const FAR_LIMIT: &str = "■ You've hit your usage limit. Upgrade to Pro, or try again at Sep 19th, 2099 6:43 PM.";

    /// 一顆在跑的本機 codex bot，畫面上是 `screen`。
    async fn codex_run(env: &tt::Env, identity: Option<&str>, screen: &str) -> (crate::db::Bot, String) {
        let app = env.app.clone();
        let bot = tt::claude_bot(&app, &env.project_id, "cx").await;
        sqlx::query("UPDATE bots SET kind='codex', identity=? WHERE id=?").bind(identity).bind(&bot.id).execute(&app.db).await.unwrap();
        let run = tt::fake_run(&app, &bot.id).await;
        env.herdr.set_screen(&format!("pane-{}", bot.id), screen);
        (crate::db::bot(&app.db, &bot.id).await.unwrap().unwrap(), run)
    }

    fn banner_screen() -> String {
        format!("› Reply with PONG\n\n{FAR_LIMIT}\n\n› \n")
    }

    async fn limit_hits(app: &Arc<App>) -> Vec<String> {
        app.quotas.lock().await.iter().filter(|(_, q)| q.limit_hit.is_some()).map(|(k, _)| k.clone()).collect()
    }

    /// #198：畫面上的撞限橫幅記不進正確那一格（身分表還沒偵測完，算不出 `cx0` 是不是預設帳號）就欠著，不猜一格
    /// `codex:cx0`；回合照樣收成 failed，派送前照欠著的那一筆擋。
    #[tokio::test]
    async fn a_limit_notice_whose_key_is_unknown_is_owed_not_guessed() {
        let env = tt::env().await;
        let app = env.app.clone();
        let (bot, run) = codex_run(&env, Some("cx0"), &banner_screen()).await;
        let turn = crate::lifecycle::run_state::a_turn(&app, &bot.id, Some(&run), "in_flight").await;

        assert!(capture_codex_usage_notices(&app, &bot.id, &run).await.is_err(), "記不進去要回錯");
        assert_eq!(limit_hits(&app).await, Vec::<String>::new(), "沒有猜一格寫下去");
        assert_eq!(crate::lifecycle::run_state::turn_status(&app, &turn).await, "failed", "回合照樣收");
        assert!(crate::quota::try_limit_hit_for_bot(&app, &bot).await.unwrap().is_some(), "欠著照擋");
    }

    /// #198 的留言：通知訊息先寫，之後這一則就不是新的。以前寫完訊息才讀「有沒有回合在飛」，那一下讀錯就回錯——沒有在飛
    /// 回合的話，下一次被當成看過的舊橫幅跳過，撞限永遠不記。現在先讀再寫；訊息寫下之後的失敗（這裡是蓋憑據）由欠帳接手。
    #[tokio::test]
    async fn a_limit_notice_is_never_lost_to_a_read_error_after_it_was_written() {
        let env = tt::env().await;
        let app = env.app.clone();
        let (bot, run) = codex_run(&env, None, &banner_screen()).await;
        // 這個 run 已經讀過一次畫面（那時沒有橫幅），這張是新的。
        super::super::limit_banner::sighting(&run, "", FAR_LIMIT, &[]);
        let a = app.clone();
        super::super::race_point::arm("codex_notice_after_insert", &bot.id, move || async move {
            sqlx::query("ALTER TABLE turns RENAME TO turns_unreadable").execute(&a.db).await.unwrap();
        });
        let first = capture_codex_usage_notices(&app, &bot.id, &run).await;
        sqlx::query("ALTER TABLE turns_unreadable RENAME TO turns").execute(&app.db).await.unwrap();
        assert!(first.is_err(), "後面寫不進去要回錯");
        capture_codex_usage_notices(&app, &bot.id, &run).await.unwrap();

        let notes: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM messages WHERE role='system' AND content LIKE '%hit your usage limit%'").fetch_one(&app.db).await.unwrap();
        assert_eq!(notes, 1, "通知只寫一次");
        assert!(crate::quota::try_limit_hit_for_bot(&app, &bot).await.unwrap().is_some(), "撞限記下了（或欠著照擋），沒有因為讀錯就丟掉");
        assert_eq!(limit_hits(&app).await, vec!["codex".to_string()]);
    }

    /// 還沒過期的同一張橫幅照舊不算新證據（2026-09-13：22:21 掃到 22:15 的舊橫幅，把 `at` 蓋成現在）。
    #[tokio::test]
    async fn an_unexpired_banner_seen_again_is_still_not_new_evidence() {
        let env = tt::env().await;
        let app = env.app.clone();
        apply(&app).await;
        let first = app.quotas.lock().await.get("codex").unwrap().limit_hit.clone().unwrap();

        apply(&app).await;

        let again = app.quotas.lock().await.get("codex").unwrap().limit_hit.clone().unwrap();
        assert_eq!((first.at, first.until), (again.at, again.until));
    }
}
