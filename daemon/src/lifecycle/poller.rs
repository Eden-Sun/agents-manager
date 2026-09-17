//! The terminal-side safety net for a turn: progress, stall, fallback and hookless capture.
//!
//! `arm_progress`, `pane_awaits_input`, `arm_fallback`/`try_fallback` and the hookless capture are
//! one chain with `hookrecv`'s late-hook write-back (a turn closed here with no reply must still be
//! fillable by a hook that arrives afterwards); they are kept together on purpose.

use super::*;


/// Unchanged polls at an empty composer before we complete the Turn ourselves (~14s; a slow hook still wins).
pub(crate) const IDLE_POLLS: u32 = 20;

/// 這回合畫面從沒出現過任何東西時的門檻（~63s）。2026-09-13（GROK、w168:pN）：grok 還沒印第一個字，
/// 空 `❯` 被當成等輸入，備援關掉回合，36 秒後 Stop hook 撞上已關的回合。等久只是備援晚接手；
/// 等不夠會吃掉使用者的問題。
pub(crate) const IDLE_POLLS_SILENT: u32 = 90;

const PROGRESS_INTERVAL: Duration = Duration::from_millis(700);
const PROGRESS_MAX: Duration = Duration::from_secs(40 * 60);

/// Min gap between `turn_progress` frames per run (4/s, docs/API.md). The poll interval isn't a
/// rate limit: `arm_progress` re-arms per turn, so turn churn could burst.
const PROGRESS_MIN_GAP: Duration = Duration::from_millis(250);

const PROGRESS_STALE: Duration = Duration::from_secs(60);

/// Split out of `flush_progress` so the budget rule is testable without an `App`.
fn progress_due(last: Option<&std::time::Instant>, force: bool) -> bool {
    force || last.is_none_or(|t| t.elapsed() >= PROGRESS_MIN_GAP)
}

/// Ship the held frame if the 4/s budget allows. Frames merge (newest wins); `force` flushes the
/// final state once the poller is done.
async fn flush_progress(app: &Arc<App>, run_id: &str, pending: &mut Option<Value>, force: bool) {
    let Some(frame) = pending.take() else { return };
    let mut emitted = app.progress_emitted.lock().await;
    if !progress_due(emitted.get(run_id), force) {
        *pending = Some(frame);
        return;
    }
    emitted.insert(run_id.to_string(), std::time::Instant::now());
    // The map outlives its poller (a new turn must not get a fresh budget); drop stale entries.
    emitted.retain(|_, t| t.elapsed() < PROGRESS_STALE);
    drop(emitted);
    app.emit("turn_progress", frame).await;
}

/// While a turn is in flight, poll the pane and push partial replies as `turn_progress`. Also the
/// idle-prompt safety net that calls `try_fallback` (see below). Stops once the turn leaves `in_flight`.
pub async fn arm_progress(app: &Arc<App>, run_id: &str, bot_id: &str, turn_id: &str) {
    // A new turn is starting: whatever cut the *previous* one short is history (§4.3a).
    crate::turn_error::clear(app, run_id, bot_id).await;
    let mut pollers = app.progress_pollers.lock().await;
    if let Some(h) = pollers.remove(run_id) {
        h.abort();
    }
    let app2 = app.clone();
    let run_id = run_id.to_string();
    let bot_id = bot_id.to_string();
    let turn_id = turn_id.to_string();
    let key = run_id.clone();
    let h = tokio::spawn(async move {
        let started = std::time::Instant::now();
        let Ok(Some(bot)) = db::bot(&app2.db, &bot_id).await else { return };
        // What we sent, to strip the pane's echo off every frame.
        let sent = turn_echo_texts(&app2, &turn_id).await;
        let mut last = (String::new(), String::new(), String::new());
        let mut quiet = 0u32;
        let mut pending: Option<Value> = None;
        loop {
            tokio::time::sleep(PROGRESS_INTERVAL).await;
            if started.elapsed() > PROGRESS_MAX {
                break;
            }
            let still = matches!(db::in_flight_turn(&app2.db, &run_id).await, Ok(Some(t)) if t.id == turn_id);
            if !still {
                break;
            }
            let Ok(Some(run)) = db::run(&app2.db, &run_id).await else { break };
            let Some(pane) = run.pane_id.clone() else { continue };
            let Ok(client) = client_for_run(&app2, &run).await else { continue };
            let Ok(read) = client.pane_read(&pane, "recent_unwrapped", 160).await else { continue };
            let live = live_reply(&bot.kind, &read.text).unwrap_or_default();
            let live = sent.iter().fold(live, |acc, p| strip_echoed_prompt(&acc, p));
            // `clean_screen` drops the spinner row, so a thinking / tool phase needs its own `activity` field.
            let activity = live_activity(&bot.kind, &read.text).unwrap_or_default();
            let alert = live_alert(&bot.kind, &read.text).unwrap_or_default();
            if live != last.0 || activity != last.1 || alert != last.2 {
                last = (live.clone(), activity.clone(), alert.clone());
                quiet = 0;
                pending = Some(
                    json!({"bot_id": bot_id, "run_id": run_id, "turn_id": turn_id, "text": live, "activity": activity, "alert": alert, "revision": read.revision}),
                );
                flush_progress(&app2, &run_id, &mut pending, false).await;
                continue;
            }
            flush_progress(&app2, &run_id, &mut pending, false).await;
            // §4.3's fallback is armed by herdr's `working -> idle`; when that sticks (2026-09-06: grok
            // idle at an empty composer, herdr still `working`) nothing closes the turn. So trust the
            // pane too: empty composer + no change = wants input. `blocked` excluded (a modal isn't an end).
            let agent_status = match db::run(&app2.db, &run_id).await {
                Ok(Some(r)) => r.agent_status,
                _ => String::new(),
            };
            if agent_status == "blocked" || !pane_awaits_input(&bot.kind, &read.text) {
                quiet = 0;
                continue;
            }
            quiet += 1;
            // 這回合畫面上出現過任何東西嗎（回覆、spinner、警示都算）。
            let said_something = !(last.0.is_empty() && last.1.is_empty() && last.2.is_empty());
            if quiet >= idle_threshold(said_something, &agent_status) {
                tracing::info!(turn = %turn_id, "pane idle at an empty prompt; completing via fallback");
                // Under the bot lock, like the Stop hook: outside it the two interleaved into two
                // assistant messages for one turn (review 2026-09-12 #5).
                let done = {
                    let lock = app2.bot_lock(&bot_id).await;
                    let _g = lock.lock().await;
                    try_fallback(&app2, &run_id).await
                };
                match done {
                    Ok(true) => break,
                    // Nothing claimed (spinner, tool, or hook won). Keep watching: this is the net for a
                    // status that never flips (grok 2026-09-06); the `still` check ends it once closed.
                    Ok(false) => quiet = 0,
                    Err(e) => {
                        tracing::debug!(turn = %turn_id, error = ?e, "idle-prompt fallback failed; keeping the poller");
                        quiet = 0;
                    }
                }
            }
        }
        flush_progress(&app2, &run_id, &mut pending, true).await;
        app2.progress_pollers.lock().await.remove(&run_id);
    });
    pollers.insert(key, h);
}


/// The user typed straight into the pane: open the `external` turn on the `-> working` edge so it
/// gets the same live bubble / progress as a web prompt. `delivery='ok'` so the §4.3 fallback
/// (which ignores other values) can still close it if the Stop hook never comes.
pub async fn begin_external_turn(app: &Arc<App>, run: &db::Run) {
    let lock = app.bot_lock(&run.bot_id).await;
    let _g = lock.lock().await;
    // Re-read under the lock: `prompt()` may have opened a turn, and the watcher is armed before
    // the run is `running`, so a boot-time `-> working` blip is not the user typing.
    let Ok(Some(run)) = db::run(&app.db, &run.id).await else { return };
    if run.state != "running" {
        return;
    }
    if !matches!(db::in_flight_turn(&app.db, &run.id).await, Ok(None)) {
        return;
    }
    let conv = match db::conversation_id(&app.db, &run.bot_id).await {
        Ok(conv) => conv,
        Err(error) => {
            tracing::warn!(error = ?error, bot = %run.bot_id, "could not get conversation for external turn");
            return;
        }
    };
    let tid = db::ulid();
    if let Err(e) = sqlx::query(
        "INSERT INTO turns (id, conversation_id, run_id, origin, status, delivery, created_at)
         VALUES (?,?,?,'external','in_flight','ok',?)",
    )
    .bind(&tid)
    .bind(&conv)
    .bind(&run.id)
    .bind(db::now())
    .execute(&app.db)
    .await
    {
        // Losing the `turns_one_in_flight` race means someone else opened it — fine.
        tracing::debug!(run = %run.id, error = %e, "external turn not opened");
        return;
    }
    tracing::info!(run = %run.id, turn = %tid, "external turn opened from pane activity");
    // No echo on screen: open the turn anyway rather than invent a user message.
    if let Some(text) = pane_prompt_echo(app, &run).await {
        if let Err(e) = insert_message(app, &conv, Some(&tid), "user", &text, "hook", false, None).await {
            tracing::warn!(turn = %tid, error = ?e, "external prompt echo not stored");
        }
    }
    emit_turn(app, &tid).await;
    arm_progress(app, &run.id, &run.bot_id, &tid).await;
}

async fn pane_prompt_echo(app: &Arc<App>, run: &db::Run) -> Option<String> {
    let bot = db::bot(&app.db, &run.bot_id).await.ok().flatten()?;
    let pane = run.pane_id.clone()?;
    let client = client_for_run(app, run).await.ok()?;
    let read = client.pane_read(&pane, "recent_unwrapped", 160).await.ok()?;
    last_prompt_echo_text(&bot.kind, &read.text)
}

/// Everything printed since the prompt echo, chrome and reply markers (`⏺ ` / `• `) removed.
pub(crate) fn live_reply(kind: &str, text: &str) -> Option<String> {
    let cleaned = clean_screen(kind, text)?;
    let out: Vec<String> = cleaned
        .lines()
        .filter(|l| !matches!(l.trim(), "⏺" | "•"))
        .map(|l| {
            let t = l.trim_start();
            t.strip_prefix("⏺ ").or_else(|| t.strip_prefix("• ")).unwrap_or(l).to_string()
        })
        .collect();
    let joined = out.join("\n").trim().to_string();
    if joined.is_empty() { None } else { Some(joined) }
}

pub(crate) const ACTIVITY_MAX: usize = crate::capture::ACTIVITY_MAX;

/// Empty composer = waiting for input? The bare marker row after stripping box frames (grok's
/// `│ ❯ │` never matches `clean_screen`'s test). Also true while claude works under a spinner, so
/// only meaningful with "nothing changed" (the progress poller's idle net → `try_fallback`).
pub(crate) fn pane_awaits_input(kind: &str, text: &str) -> bool {
    if kind == "claude" {
        return crate::capture::claude::PARSER.awaits_input(text);
    }
    let Some(marker) = prompt_echo_prefix(kind).and_then(|p| p.trim_end().chars().next()) else { return false };
    text.lines().rev().take(12).any(|l| {
        let mut chars = l.chars().filter(|c| !"│┃╭╮╰╯─━ \t".contains(*c));
        chars.next() == Some(marker) && chars.next().is_none()
    })
}

/// 空 composer 靜止幾輪才算等輸入。herdr 說閒著且畫面印過東西 → 14 秒（2026-09-06 grok 卡 working
/// 的安全網）；herdr 說 working 或這回合什麼都沒印過 → 63 秒（2026-09-13 GROK 還在想就被關）。
pub(crate) fn idle_threshold(said_something: bool, agent_status: &str) -> u32 {
    if said_something && agent_status != "working" {
        IDLE_POLLS
    } else {
        IDLE_POLLS_SILENT
    }
}

/// Every form of what we sent, for echo matching: image prompts were delivered with
/// `attach::deliver_text`'s appendix, which came back stored as an answer
/// (`01M1XSVME9SKEG1NZXG51HFP73`, 2026-09-07). Delivered form first so the longer text strips first.
///
/// 第一個是 `turns.prompt_text`——**實際打給 agent 的字**。訊息泡泡存的是使用者看到的原文：群組訊息帶著 @mention，
/// 拿它去搜畫面一定找不到、重送時還把路由語法打進 pane（review3 c3 M4）。沒有 `prompt_text` 的舊列才退回從訊息重算。
pub(crate) async fn turn_echo_texts(app: &Arc<App>, turn_id: &str) -> Vec<String> {
    let rows = db::turn_user_messages_with_attachments(&app.db, turn_id).await.unwrap_or_default();
    let mut out = Vec::new();
    let delivered: Option<String> = sqlx::query_scalar("SELECT prompt_text FROM turns WHERE id = ?")
        .bind(turn_id)
        .fetch_optional(&app.db)
        .await
        .ok()
        .flatten()
        .flatten();
    if let Some(p) = delivered.filter(|p| !p.trim().is_empty()) {
        out.push(p);
    }
    for (content, attachments) in rows {
        if let Some(json) = attachments.as_deref() {
            if let Ok(items) = serde_json::from_str::<Vec<crate::attach::Attachment>>(json) {
                let delivered = crate::attach::deliver_text(&content, &items);
                if delivered != content && !out.contains(&delivered) {
                    out.push(delivered);
                }
            }
        }
        if !out.contains(&content) {
            out.push(content);
        }
    }
    out
}

/// The reply in this snapshot with our prompt echo removed, or `None`. The three strippers only
/// mean "the agent said something" together; our own prompt alone is not an answer.
pub(crate) fn screen_reply(kind: &str, text: &str, sent: &[String]) -> Option<String> {
    let raw = extract_reply(kind, text).or_else(|| clean_screen(kind, text))?;
    let stripped = sent.iter().fold(raw, |acc, p| strip_echoed_prompt(&acc, p));
    let stripped = stripped.trim();
    if stripped.is_empty() {
        None
    } else {
        Some(stripped.to_string())
    }
}

/// Trailing-ellipsis marker a TUI leaves where it clipped the echo of a long prompt.
const ELLIPSES: [&str; 2] = ["…", "..."];

fn without_ellipsis(s: &str) -> Option<&str> {
    ELLIPSES.iter().find_map(|e| s.strip_suffix(e)).map(str::trim_end)
}

fn squash(s: &str) -> String {
    s.chars().filter(|c| !c.is_whitespace()).collect()
}

/// Is this line "the rest of the prompt, clipped"? grok squeezes lines 2..n onto one row ending in
/// `…`. Requiring a prefix of the remaining prompt (≥ [`SQUASH_MIN`] chars) keeps real replies ending in `…` safe.
pub(crate) fn is_clipped_echo(line: &str, rest: &[&str]) -> bool {
    let Some(body) = without_ellipsis(line.trim()) else { return false };
    let body = squash(body);
    if body.chars().count() < SQUASH_MIN {
        return false;
    }
    squash(&rest.join("")).starts_with(&body)
}

/// Drop lines 2..n of a multi-line prompt echo: `after_last_prompt_echo` only skips the `❯` row.
/// Matching against what we sent is exact, unlike guessing from indentation.
pub(crate) fn strip_echoed_prompt(text: &str, prompt: &str) -> String {
    let want: Vec<&str> = prompt.lines().map(str::trim).filter(|l| !l.is_empty()).collect();
    if want.len() < 2 {
        // A one-line prompt can still be shredded across rows in a narrow pane.
        return strip_echoed_prompt_squashed(text, prompt).unwrap_or_else(|| text.to_string());
    }
    let lines: Vec<&str> = text.lines().collect();
    let mut i = 0;
    // The `❯ <first line>` marker row is already gone, so resume at the prompt's second line.
    let mut w = 1;
    while i < lines.len() && w < want.len() {
        let l = lines[i].trim();
        if l.is_empty() {
            i += 1;
            continue;
        }
        if l != want[w] {
            // The TUI may have clipped the whole remaining echo onto this row.
            if is_clipped_echo(l, &want[w..]) {
                i += 1;
                w = want.len();
            }
            break;
        }
        i += 1;
        w += 1;
    }
    // Only strip on a full match; eating half a real reply is worse than an echo.
    if w < want.len() {
        return strip_echoed_prompt_squashed(text, prompt).unwrap_or_else(|| text.to_string());
    }
    lines[i..].join("\n").trim().to_string()
}

/// Whitespace-insensitive fallback for [`strip_echoed_prompt`]: a very narrow pane lays the echo
/// out one char per line, so it was stored as a vertical-column reply. The candidate must open
/// with ≥ [`SQUASH_MIN`] chars of the prompt tail to keep real replies safe.
const SQUASH_MIN: usize = 8;

fn strip_echoed_prompt_squashed(text: &str, prompt: &str) -> Option<String> {
    let ps: Vec<char> = prompt.chars().filter(|c| !c.is_whitespace()).collect();
    let ts: Vec<char> = text.chars().filter(|c| !c.is_whitespace()).collect();
    if ps.len() < SQUASH_MIN || ts.is_empty() {
        return None;
    }
    // Longest prompt tail the candidate opens with (the head went with the `❯ ` row).
    let mut matched = 0;
    for k in 0..ps.len() {
        let suf = &ps[k..];
        if suf.len() < SQUASH_MIN {
            break;
        }
        if ts.len() >= suf.len() {
            if ts[..suf.len()] == *suf {
                matched = suf.len();
                break;
            }
        } else if suf[..ts.len()] == *ts {
            // The candidate ran out inside the echo: all of it is echo.
            return Some(String::new());
        }
    }
    if matched == 0 {
        return None;
    }
    let mut n = 0;
    let mut cut = text.len();
    for (i, c) in text.char_indices() {
        if n == matched {
            cut = i;
            break;
        }
        if !c.is_whitespace() {
            n += 1;
        }
    }
    if n < matched {
        return Some(String::new());
    }
    Some(text[cut..].trim().to_string())
}

/// Pane width for the "too narrow" message; best effort, failure only means less detail.
async fn pane_columns(app: &Arc<App>, run: &db::Run) -> Option<u32> {
    let pane = run.pane_id.clone()?;
    let ws = run.workspace_id.clone()?;
    let client = client_for_run(app, run).await.ok()?;
    let rects = client.pane_rects(&ws).await.ok()?;
    rects.into_iter().find(|(id, _, _)| *id == pane).map(|(_, w, _)| w)
}

/// Output shredded into single characters? herdr unwraps terminal wrapping, not the TUI's own
/// one-glyph-per-row layout, whose spaces are lost — say so rather than store a column.
pub(crate) fn is_shredded(text: &str) -> bool {
    crate::capture::is_shredded(text)
}

/// Spinner-row shape `<Verb>… (3m 18s · ↓ 11.0k tokens)`. Verb is random and glyphs change
/// across releases; only the bracketed time / token counter is stable.
pub(crate) fn is_activity_shape(s: &str) -> bool {
    crate::capture::is_activity_shape(s)
}

/// The spinner row of this turn, glyph stripped, for `turn_progress.activity` only — never stored,
/// so `clean_screen` / `extract_reply` can keep dropping it as chrome.
pub(crate) fn live_activity(kind: &str, text: &str) -> Option<String> {
    if kind == "claude" {
        return crate::capture::claude::PARSER.activity(text);
    }
    let lines: Vec<&str> = text.lines().collect();
    let start = after_last_prompt_echo(kind, &lines);
    let grok = kind == "grok";
    // Fast path only; the glyph set grows between releases, `is_activity_shape` catches the rest.
    let glyphs: &[char] = if grok { &['◆'] } else { &['✻', '✽', '✶', '✳', '✢', '·'] };
    let mut found: Option<String> = None;
    for line in &lines[start..] {
        let stripped;
        let s = if grok {
            stripped = strip_grok_decor(line);
            stripped.trim()
        } else {
            line.trim()
        };
        let Some(first) = s.chars().next() else { continue };
        let rest = if glyphs.contains(&first) {
            s[first.len_utf8()..].trim()
        } else if is_activity_shape(s) {
            // Unknown glyph (or none at all): drop a leading symbol if there is one.
            if first.is_alphanumeric() { s } else { s[first.len_utf8()..].trim() }
        } else {
            continue;
        };
        if rest.is_empty() {
            continue;
        }
        // The last activity row on screen is the current one.
        found = Some(rest.to_string());
    }
    let s = found?;
    if s.chars().count() <= ACTIVITY_MAX {
        return Some(s);
    }
    let mut cut: String = s.chars().take(ACTIVITY_MAX).collect::<String>().trim_end().to_string();
    cut.push('…');
    Some(cut)
}

/// Retry / API-error banner (`API error · Retrying in 3s · attempt 1/10`, codex `stream error: …;
/// retrying 2/5`) as `turn_progress.alert`, since the spinner makes a stuck turn look healthy.
/// Shape, not wording: short + *error* + retry/attempt token, or opens with `API error`.
pub(crate) fn live_alert(kind: &str, text: &str) -> Option<String> {
    const RETRY_TOKENS: [&str; 6] = ["retry", "retrying", "attempt", "reconnect", "重試", "retries"];
    // Codex hard limit may wrap across narrow-pane rows: multi-line scanner.
    if kind == "codex" {
        if let Some(hit) = codex_usage_notice_lines(text)
            .into_iter()
            .find(|n| codex_limit_hit_line(n).is_some())
        {
            if hit.chars().count() <= ACTIVITY_MAX {
                return Some(hit);
            }
            let mut cut: String = hit.chars().take(ACTIVITY_MAX).collect::<String>().trim_end().to_string();
            cut.push('…');
            return Some(cut);
        }
    }
    let lines: Vec<&str> = text.lines().collect();
    let start = after_last_prompt_echo(kind, &lines);
    let mut found: Option<String> = None;
    for line in &lines[start..] {
        let stripped;
        let s = if kind == "grok" {
            stripped = strip_grok_decor(line);
            stripped.trim()
        } else {
            line.trim()
        };
        // Drop a leading spinner / bullet glyph so `✻ API error …` matches too.
        let s = match s.chars().next() {
            Some(c) if !c.is_alphanumeric() => s[c.len_utf8()..].trim(),
            _ => s,
        };
        if s.is_empty() || s.chars().count() > ACTIVITY_MAX * 2 {
            continue;
        }
        let low = s.to_ascii_lowercase();
        let says_error = low.contains("error") || low.contains("錯誤") || low.contains("overloaded");
        if !says_error {
            continue;
        }
        // Data, not a banner (2026-09-08): JSON or `|` rows from e.g. a sqlite dump.
        if s.contains('{') || s.contains('}') || s.matches('|').count() >= 2 {
            continue;
        }
        let retrying = RETRY_TOKENS.iter().any(|t| low.contains(t));
        if !retrying && !low.starts_with("api error") {
            continue;
        }
        // The newest banner is the one still true.
        found = Some(s.to_string());
    }
    let s = found?;
    if s.chars().count() <= ACTIVITY_MAX {
        return Some(s);
    }
    let mut cut: String = s.chars().take(ACTIVITY_MAX).collect::<String>().trim_end().to_string();
    cut.push('…');
    Some(cut)
}


const STALL_SECS: u64 = 12;

/// Rows above the bottom the composer can start (it grows with its text; claude adds a hint row).
pub(crate) const COMPOSER_TAIL: usize = 24;

/// Chars of our prompt head that must be visible in the composer. `contains`, not prefix: claude
/// puts `[Image #6]` in front of pasted text.
const COMPOSER_HEAD: usize = 12;

/// Never match on a fragment this short — a two-character prompt is in every screen.
const COMPOSER_HEAD_MIN: usize = 4;

fn undecorate_row(line: &str) -> String {
    let s = strip_grok_decor(line);
    s.trim().trim_start_matches('│').trim_end_matches('│').trim().to_string()
}

fn is_rule_row(s: &str) -> bool {
    !s.is_empty() && s.chars().all(|c| "─━-=_╭╮╰╯".contains(c))
}

/// Text in the input box, or `None` when empty / no known marker. Searched from the bottom; an
/// empty box is `pane_awaits_input`'s job, else we'd walk back to an accepted prompt's echo.
pub(crate) fn composer_text(kind: &str, screen: &str) -> Option<String> {
    let marker = prompt_echo_prefix(kind)?.trim_end();
    if pane_awaits_input(kind, screen) {
        return None;
    }
    let lines: Vec<&str> = screen.lines().collect();
    let from = lines.len().saturating_sub(COMPOSER_TAIL);
    let tail = &lines[from..];
    let idx = tail.iter().rposition(|l| {
        let t = undecorate_row(l);
        t.starts_with(marker) && !t[marker.len()..].trim().is_empty()
    })?;
    let mut out: Vec<String> = Vec::new();
    for (n, line) in tail[idx..].iter().enumerate() {
        let row = undecorate_row(line);
        let body = if n == 0 { row[marker.len()..].trim().to_string() } else { row };
        if n > 0 && (body.is_empty() || is_rule_row(&body)) {
            break;
        }
        out.push(body);
    }
    let joined = out.join("\n").trim().to_string();
    if joined.is_empty() {
        None
    } else {
        Some(joined)
    }
}

/// Is our prompt still unsent in the box? 2026-09-07 11:21: claude swallowed the Enter while
/// compacting; the turn failed as a stall with the message one keystroke from sent.
pub(crate) fn composer_holds_prompt(kind: &str, screen: &str, sent: &str) -> bool {
    let Some(box_text) = composer_text(kind, screen) else { return false };
    let needle: String = squash(sent).chars().take(COMPOSER_HEAD).collect();
    if needle.chars().count() < COMPOSER_HEAD_MIN {
        return false;
    }
    squash(&box_text).contains(&needle)
}

/// Early check after delivery: enough for the box to draw, short of the full stall deadline.
const NUDGE_EARLY_SECS: u64 = 3;

/// After re-sending Enter, how long the agent gets to react before the turn is failed.
const NUDGE_GRACE_SECS: u64 = 8;

/// Press Enter if our prompt is still in the box and the agent idle. Best effort: every reason to
/// do nothing is `false`, never an error.
async fn nudge_unsent_prompt(app: &Arc<App>, run_id: &str, turn_id: &str, sent: &[String]) -> bool {
    let Ok(Some(run)) = db::run(&app.db, run_id).await else { return false };
    if run.agent_status == "working" || run.agent_status == "blocked" {
        return false;
    }
    if !matches!(db::in_flight_turn(&app.db, run_id).await, Ok(Some(t)) if t.id == turn_id && t.delivery == "ok") {
        return false;
    }
    let Ok(Some(bot)) = db::bot(&app.db, &run.bot_id).await else { return false };
    let Some(pane) = run.pane_id.clone() else { return false };
    let Ok(client) = client_for_run(app, &run).await else { return false };
    let Ok(read) = client.pane_read(&pane, "visible", 80).await else { return false };
    if !sent.iter().any(|p| composer_holds_prompt(&bot.kind, &read.text, p)) {
        return false;
    }
    if let Err(e) = client.pane_send_keys(&pane, &["Enter"]).await {
        tracing::warn!(error = ?e, run_id, "could not re-send Enter for an unsent prompt");
        return false;
    }
    tracing::warn!(run_id, turn = %turn_id, "prompt was still in the composer; re-sent Enter");
    true
}

/// How many times one turn may be re-delivered because the prompt never showed up on screen.
/// One: a second loss means something is really wrong with the pane, and repeating a prompt the
/// agent *did* get is worse than failing the turn.
pub(crate) const MAX_PROMPT_RESENDS: i64 = 1;

/// Scrollback searched for our prompt before deciding it never arrived. Generous on purpose: a
/// resend is only safe when the echo is truly nowhere, not merely scrolled off the visible rows.
const RESEND_SCAN_LINES: u32 = 400;

/// Did our prompt never reach the TUI at all? An empty composer **and** no trace of the prompt
/// anywhere in the scrollback — not in the box (that is [`composer_holds_prompt`]'s Enter nudge),
/// not as a submitted `❯ …` row, not as a queued message.
///
/// 2026-09-14 w1HJ:pH (wits-c1-op-xh)：使用者在 UI 改 effort，daemon 對 pane 打 `/effort max`
/// 當場套用；4 秒後的 prompt 由 herdr 回報送達（`delivery=ok`），畫面上卻一個字都沒有，transcript
/// 也沒有這則，12 秒後被判 stall。畫面停在 `/effort max` 的回饋加上空的 `❯`。
pub(crate) fn prompt_never_reached_screen(kind: &str, screen: &str, sent: &[String]) -> bool {
    if !pane_awaits_input(kind, screen) {
        return false;
    }
    let hay = squash(screen);
    let mut any_needle = false;
    for p in sent {
        let needle: String = squash(p).chars().take(COMPOSER_HEAD).collect();
        if needle.chars().count() < COMPOSER_HEAD_MIN {
            continue;
        }
        any_needle = true;
        if hay.contains(&needle) {
            return false;
        }
    }
    // Nothing long enough to look for: cannot prove absence, so never resend.
    any_needle
}

/// What a resend attempt came to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Resend {
    /// Re-delivered.
    Sent,
    /// Nothing to do (guards, the prompt is on screen after all, budget spent, the send failed).
    Skipped,
    /// Refused before a single byte was typed (`Delivered::NotAttempted`); the budget was given back.
    Blocked { reason: &'static str, retry: bool },
}

impl Resend {
    fn sent(self) -> bool {
        self == Resend::Sent
    }
}

/// How long the stall watchdog waits before it retries a resend that was blocked before typing
/// (the box had a draft for a moment, the transcript could not be read yet).
const RESEND_RETRY_SECS: u64 = 3;

/// Re-deliver a prompt that never reached the pane, at most [`MAX_PROMPT_RESENDS`] times per turn.
/// Same guards as [`nudge_unsent_prompt`]; every reason to do nothing is `Skipped`.
async fn resend_lost_prompt(app: &Arc<App>, run_id: &str, turn_id: &str, sent: &[String]) -> Resend {
    let Ok(Some(run)) = db::run(&app.db, run_id).await else { return Resend::Skipped };
    if run.agent_status == "working" || run.agent_status == "blocked" {
        return Resend::Skipped;
    }
    // 重送看 `auto_resend`，不看有沒有證據（AGM 2026-09-16）：打過字但證不明的那條路重送會重複派工，
    // 所以它是 0；`agent.prompt` 一樣沒有證據，但它沒送進去才會走到這裡，重送是安全的。
    if !matches!(db::in_flight_turn(&app.db, run_id).await, Ok(Some(t)) if t.id == turn_id && t.delivery == "ok" && t.auto_resend != 0) {
        return Resend::Skipped;
    }
    let Ok(Some(bot)) = db::bot(&app.db, &run.bot_id).await else { return Resend::Skipped };
    let Some(pane) = run.pane_id.clone() else { return Resend::Skipped };
    let Ok(client) = client_for_run(app, &run).await else { return Resend::Skipped };
    let Ok(read) = client.pane_read(&pane, crate::lifecycle::delivery::SCAN_SOURCE, RESEND_SCAN_LINES).await else { return Resend::Skipped };
    if !prompt_never_reached_screen(&bot.kind, &read.text, sent) {
        return Resend::Skipped;
    }
    // `sent[0]` is the delivered form (`turns.prompt_text`: mentions stripped, attachment paths
    // included), exactly what was sent before.
    let Some(text) = sent.first() else { return Resend::Skipped };
    // The claim is the lock and it lives in the DB: a queue flush and this watchdog cannot both
    // resend, and a daemon restart does not hand the same turn a fresh budget (sol review #3).
    if !db::claim_resend(&app.db, turn_id, MAX_PROMPT_RESENDS).await {
        return Resend::Skipped;
    }
    // The first delivery just failed silently; re-deliver the way that is verified on screen.
    let res = deliver_prompt(app, &client, &run, &bot, text, true, true).await;
    match res {
        Ok(Delivered::Submitted | Delivered::Handed) => {
            tracing::warn!(run_id, turn = %turn_id, bot = %bot.name,
                           "prompt never reached the pane (empty composer, no echo in scrollback); re-delivered it");
            Resend::Sent
        }
        Ok(Delivered::Unverified) => {
            tracing::warn!(run_id, turn = %turn_id, "prompt re-delivered without lossless evidence");
            Resend::Sent
        }
        Ok(Delivered::NotAttempted { reason, retry }) => {
            // 一個字都沒寫進去（`NotAttempted` 的契約），所以這次不該算進唯一一次重送額度：
            // 擋下它的原因（框裡剛好有字）通常兩秒後就消失了——呼叫端（`arm_stall`）會再試一次。
            db::refund_resend(&app.db, turn_id).await;
            tracing::warn!(run_id, turn = %turn_id, reason, "re-delivery was not attempted; the resend budget is given back");
            Resend::Blocked { reason, retry }
        }
        Ok(Delivered::Unproven(why)) => {
            tracing::warn!(run_id, turn = %turn_id, reason = why, "re-delivery could not be proven either");
            Resend::Skipped
        }
        Err(e) => {
            tracing::warn!(run_id, turn = %turn_id, error = %e, "re-delivering a lost prompt failed");
            Resend::Skipped
        }
    }
}

/// The watchdog's resend, with one retry when the first try was refused before typing anything.
///
/// Before this, a refunded budget was never used: `arm_stall` failed the turn right after the refusal,
/// and nothing re-arms a watchdog later (review2 deliv #4). `None` = the stall timer was replaced
/// meanwhile (the caller stops); otherwise `(re-delivered, what blocked the last try)`.
async fn resend_with_retry(
    app: &Arc<App>,
    run_id: &str,
    bot_id: &str,
    turn_id: &str,
    sent: &[String],
    generation: u64,
    retry_after: Duration,
) -> Option<(bool, Option<&'static str>)> {
    let mut blocked = None;
    for attempt in 0..2 {
        if attempt > 0 {
            tokio::time::sleep(retry_after).await;
        }
        let lock = app.bot_lock(bot_id).await;
        let _g = lock.lock().await;
        if app.stall_timers.lock().await.get(run_id) != Some(&generation) {
            return None;
        }
        match resend_lost_prompt(app, run_id, turn_id, sent).await {
            Resend::Sent => return Some((true, None)),
            Resend::Skipped => return Some((false, blocked)),
            Resend::Blocked { reason, retry } => {
                blocked = Some(reason);
                if !retry {
                    break;
                }
            }
        }
    }
    Some((false, blocked))
}

/// The agent must leave `idle` within `STALL_SECS` after delivery, or the Turn sits `in_flight`
/// forever (not logged in, invisible modal). Cancelled by the first `working` / `blocked` event.
pub async fn arm_stall(app: &Arc<App>, run_id: &str, bot_id: &str, turn_id: &str) {
    let mut timers = app.stall_timers.lock().await;
    static NEXT_GENERATION: AtomicU64 = AtomicU64::new(0);
    let generation = NEXT_GENERATION.fetch_add(1, Ordering::Relaxed);
    timers.insert(run_id.to_string(), generation);
    let app2 = app.clone();
    let run_id = run_id.to_string();
    let bot_id = bot_id.to_string();
    let turn_id = turn_id.to_string();
    tokio::spawn(async move {
        let sent = turn_echo_texts(&app2, &turn_id).await;
        let mut nudged;
    // Every prompt gets the early look: a single line pasted into a busy TUI loses its Enter too.
        tokio::time::sleep(Duration::from_secs(NUDGE_EARLY_SECS)).await;
        {
            let lock = app2.bot_lock(&bot_id).await;
            let _g = lock.lock().await;
            if app2.stall_timers.lock().await.get(&run_id) != Some(&generation) {
                return;
            }
            nudged = nudge_unsent_prompt(&app2, &run_id, &turn_id, &sent).await;
        }
        tokio::time::sleep(Duration::from_secs(STALL_SECS - NUDGE_EARLY_SECS)).await;
    // Deadline: if the text is still there, press Enter and grant a grace period first.
        {
            let lock = app2.bot_lock(&bot_id).await;
            let _g = lock.lock().await;
            if app2.stall_timers.lock().await.get(&run_id) != Some(&generation) {
                return;
            }
            nudged |= nudge_unsent_prompt(&app2, &run_id, &turn_id, &sent).await;
        }
        // Not in the box either: the prompt never reached the TUI. Deliver it again once instead of
        // failing the turn with a system message the user has to act on.
        let mut resent = false;
        let mut blocked = None;
        if !nudged {
            let Some((r, b)) =
                resend_with_retry(&app2, &run_id, &bot_id, &turn_id, &sent, generation, Duration::from_secs(RESEND_RETRY_SECS)).await
            else {
                return;
            };
            (resent, blocked) = (r, b);
        }
        if nudged || resent {
            tokio::time::sleep(Duration::from_secs(NUDGE_GRACE_SECS)).await;
        }
        // A resend can land in a busy box just like the first try did.
        if resent {
            let lock = app2.bot_lock(&bot_id).await;
            let _g = lock.lock().await;
            if app2.stall_timers.lock().await.get(&run_id) != Some(&generation) {
                return;
            }
            if nudge_unsent_prompt(&app2, &run_id, &turn_id, &sent).await {
                drop(_g);
                tokio::time::sleep(Duration::from_secs(NUDGE_GRACE_SECS)).await;
            }
        }
        let lock = app2.bot_lock(&bot_id).await;
        let _g = lock.lock().await;
        if app2.stall_timers.lock().await.get(&run_id) != Some(&generation) {
            return;
        }
        if let Err(e) = fail_stalled_turn(&app2, &run_id, &bot_id, &turn_id, nudged, resent, blocked).await {
            tracing::warn!(error = ?e, "stall watchdog failed");
        }
        let mut timers = app2.stall_timers.lock().await;
        if timers.get(&run_id) == Some(&generation) {
            timers.remove(&run_id);
        }
    });
}

pub async fn cancel_stall(app: &Arc<App>, run_id: &str) {
    app.stall_timers.lock().await.remove(run_id);
}

async fn fail_stalled_turn(
    app: &Arc<App>,
    run_id: &str,
    bot_id: &str,
    turn_id: &str,
    nudged: bool,
    resent: bool,
    resend_blocked: Option<&str>,
) -> anyhow::Result<()> {
    let Some(run) = db::run(&app.db, run_id).await? else { return Ok(()) };
    if run.agent_status == "working" || run.agent_status == "blocked" {
        return Ok(());
    }
    let Some(turn) = db::in_flight_turn(&app.db, run_id).await? else { return Ok(()) };
    if turn.id != turn_id || turn.delivery != "ok" {
        return Ok(());
    }
    // Quote the screen, never assert a cause (the host may well be logged in).
    let mut snapshot: Option<String> = None;
    let mut hints: Vec<String> = Vec::new();
    if let Ok(client) = client_for_run(app, &run).await {
        if let Some(pane) = run.pane_id.as_deref() {
            if let Ok(read) = client.pane_read(pane, "visible", 60).await {
                hints = stall_hint_lines(&read.text);
                snapshot = Some(read.text);
            }
        }
    }
    let mut reason = stall_reason(&hints, nudged);
    if resent {
        reason.push_str(&format!("\n（第一次送出後畫面上完全沒有這則訊息，已自動重送 {MAX_PROMPT_RESENDS} 次，仍沒有反應。）"));
    } else if let Some(why) = resend_blocked {
        // 以前這種情況的訊息完全不提試過重送：看起來像 agent 沒反應，其實是重送被擋下（review2 deliv #4）。
        reason.push_str(&format!("\n（畫面上完全沒有這則訊息；試著自動重送時被擋下（{why}），一個字都沒打，所以沒有送出。清掉擋住的東西後請重送。）"));
    }
    let mut tx = app.db.begin().await?;
    let res = sqlx::query("UPDATE turns SET status='failed', completed_at=? WHERE id=? AND status='in_flight'")
        .bind(db::now())
        .bind(turn_id)
        .execute(&mut *tx)
        .await?;
    if res.rows_affected() == 0 {
        return Ok(());
    }
    let message = insert_message_tx(
        &mut tx,
        &turn.conversation_id,
        Some(turn_id),
        "system",
        &reason,
        "system",
        false,
        snapshot.as_deref(),
    )
    .await?;
    tx.commit().await?;
    tracing::warn!(turn = %turn_id, bot = %bot_id, %reason, "prompt stalled; turn failed");
    emit_message_added(app, bot_id, message).await;
    emit_turn(app, turn_id).await;
    Ok(())
}

/// Screen lines worth quoting back to the user when a prompt stalls.
fn stall_hint_lines(screen: &str) -> Vec<String> {
    const NEEDLES: [&str; 5] = ["not logged in", "/login", "unlock-keychain", "usage limit", "limit"];
    let mut out: Vec<String> = Vec::new();
    for line in screen.lines() {
        let t = line.trim();
        if t.is_empty() {
            continue;
        }
        let low = t.to_lowercase();
        if NEEDLES.iter().any(|n| low.contains(n)) && !out.iter().any(|o| o == t) {
            out.push(t.to_string());
        }
    }
    out
}

/// Neutral wording: symptom + quoted screen + keychain caveat; never claim "not logged in".
pub(crate) fn stall_reason(hints: &[String], nudged: bool) -> String {
    let head = format!("agent 在 {STALL_SECS} 秒內沒有對訊息作出反應。");
    // `nudged`: tell the user we already pressed Enter for text left in the box.
    let head = if nudged {
        format!("{head}訊息還留在輸入框沒送出（TUI 忙碌時會把多行文字當成貼上，吞掉最後的 Enter），已嘗試補送一次 Enter，等 {NUDGE_GRACE_SECS} 秒仍沒有反應。")
    } else {
        head
    };
    if hints.is_empty() {
        return format!("{head}請查看終端分頁。");
    }
    format!(
        "{head}終端畫面：\n{}\n若該身份使用 macOS Keychain 儲存憑證，透過 ssh 啟動的 herdr 可能讀不到（畫面提示 `security unlock-keychain`）。",
        hints.join("\n")
    )
}

pub async fn abandon_turn(app: &Arc<App>, turn_id: &str) -> LcResult<()> {
    let conversation_id = sqlx::query_scalar::<_, String>("SELECT conversation_id FROM turns WHERE id=?")
        .bind(turn_id)
        .fetch_optional(&app.db)
        .await
        .map_err(up)?
        .ok_or_else(|| LcError::NotFound("turn".into()))?;
    let bot_id = sqlx::query_scalar::<_, String>("SELECT bot_id FROM conversations WHERE id=?")
        .bind(&conversation_id)
        .fetch_one(&app.db)
        .await
        .map_err(up)?;
    let lock = app.bot_lock(&bot_id).await;
    let _g = lock.lock().await;
    let t = sqlx::query_as::<_, db::Turn>("SELECT * FROM turns WHERE id=?")
        .bind(turn_id)
        .fetch_optional(&app.db)
        .await
        .map_err(up)?
        .ok_or_else(|| LcError::NotFound("turn".into()))?;
    if t.status != "in_flight" {
        return Err(LcError::conflict("turn is neither in-flight nor of unknown delivery", json!({"turn_id": t.id})));
    }
    let res = sqlx::query(
        "UPDATE turns SET status='failed', delivery = CASE WHEN delivery='unknown' THEN 'failed' ELSE delivery END, completed_at=?
         WHERE id=? AND status='in_flight'",
    )
        .bind(db::now())
        .bind(turn_id)
        .execute(&app.db)
        .await
        .map_err(up)?;
    if res.rows_affected() == 0 {
        return Err(LcError::conflict("turn is neither in-flight nor of unknown delivery", json!({"turn_id": t.id})));
    }
    let _ = insert_message(app, &t.conversation_id, Some(turn_id), "system", "turn abandoned by user", "system", false, None).await;
    emit_turn(app, turn_id).await;
    Ok(())
}


/// Arm the 5s terminal-fallback timer after a working -> idle transition.
pub async fn arm_fallback(app: &Arc<App>, run_id: &str, bot_id: &str) {
    let mut timers = app.fallback_timers.lock().await;
    static NEXT_GENERATION: AtomicU64 = AtomicU64::new(0);
    let generation = NEXT_GENERATION.fetch_add(1, Ordering::Relaxed);
    timers.insert(run_id.to_string(), generation);
    let app2 = app.clone();
    let run_id = run_id.to_string();
    let bot_id = bot_id.to_string();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_secs(5)).await;
        let lock = app2.bot_lock(&bot_id).await;
        let _g = lock.lock().await;
        if app2.fallback_timers.lock().await.get(&run_id) != Some(&generation) {
            return;
        }
        match try_fallback(&app2, &run_id).await {
            // No turn, but for a hookless run the pane is the only record of the answer.
            Ok(false) => {
                if let Err(e) = capture_hookless_turn_locked(&app2, &run_id, false).await {
                    tracing::warn!(error = ?e, "hookless terminal capture failed");
                }
            }
            Ok(true) => {}
            Err(e) => tracing::warn!(error = ?e, "terminal fallback failed"),
        }
        // A Codex usage hint may render after notify already completed the turn: capture regardless.
        if let Err(e) = capture_codex_usage_notices(&app2, &bot_id, &run_id).await {
            tracing::debug!(error = ?e, "codex notice capture failed");
        }
        // §4.3a: also classify ended vs cut off by the API, even when the Stop hook already completed it.
        if let Err(e) = crate::turn_error::capture(&app2, &bot_id, &run_id).await {
            tracing::debug!(error = ?e, "turn error capture failed");
        }
        let mut timers = app2.fallback_timers.lock().await;
        if timers.get(&run_id) == Some(&generation) {
            timers.remove(&run_id);
        }
    });
}

/// Close the in-flight turn from the pane (§4.3). `Ok(false)`: nothing to fall back on (no turn,
/// or untrusted delivery). A turn closed with zero reply can still be filled by hookrecv's late hook.
async fn try_fallback(app: &Arc<App>, run_id: &str) -> anyhow::Result<bool> {
    let Some(run) = db::run(&app.db, run_id).await? else { return Ok(false) };
    let Some(turn) = db::in_flight_turn(&app.db, run_id).await? else { return Ok(false) };
    if turn.delivery != "ok" {
        return Ok(false);
    }
    let Some(bot) = db::bot(&app.db, &run.bot_id).await? else { return Ok(false) };

    // Read the pane before claiming: a failure after claiming left `completed_fallback` with no
    // message, no `turn_updated`, no queue flush. Failing here keeps it in flight for the next edge.
    let pane_id = run.pane_id.clone().unwrap_or_default();
    let client = app
        .herdr_for_run(&run)
        .await
        .ok_or_else(|| anyhow::anyhow!("no Herdr session is available for run `{}`", run.id))?;
    let read = client.pane_read(&pane_id, "recent_unwrapped", 200).await?;

    // A spinner still turning means mid-thought whatever herdr says: claiming stored chrome as the
    // answer (2026-09-07 11:17). Left in flight, the next edge or the idle poller re-arms this.
    if pane_still_busy(&read.text) {
        tracing::debug!(run_id, "pane still shows a spinner; not completing the turn from it");
        return Ok(false);
    }
    let fresh_probe = slice_after_cursor(&read.text, run.last_read_tail_hash.as_deref());
    let probe = extract_reply(&bot.kind, &fresh_probe).or_else(|| clean_screen(&bot.kind, &fresh_probe)).unwrap_or_default();
    if is_tool_progress(&probe) {
        tracing::debug!(run_id, "pane is still mid-tool-call; not completing the turn from it");
        return Ok(false);
    }

    let fresh = slice_after_cursor(&read.text, run.last_read_tail_hash.as_deref());

    // Codex hard limit: system notice + failed turn, not a fake reply. Banner may sit above the cursor,
    // but a replayed old one must not count (`limit_banner::fallback_hit`, 2026-09-15).
    let codex_hit = if bot.kind == "codex" {
        let limits = |t: &str| -> Vec<String> {
            codex_usage_notice_lines(t).into_iter().filter(|n| codex_limit_hit_line(n).is_some()).collect()
        };
        super::limit_banner::fallback_hit(run_id, &read.text, limits(&fresh), limits(&read.text))
    } else {
        None
    };

    // Derive the reply before SQLite's write lock (`pane_columns` RPC, `turn_echo_texts` other conn).
    let reply = if codex_hit.is_some() {
        None
    } else {
    // Only the `❯` row counts as echo; lines 2..n would be stored as the answer.
        let sent = turn_echo_texts(app, &turn.id).await;
        match screen_reply(&bot.kind, &fresh, &sent) {
            None => None,
            Some(reply) => Some(if is_shredded(&reply) {
                // Shredded = too narrow to read; name the pane and width so it's actionable.
                let how_wide = match pane_columns(app, &run).await {
                    Some(w) => format!("目前 {w} 欄，"),
                    None => String::new(),
                };
                format!(
                    "（pane {pane_id} 太窄，{how_wide}輸出在終端就被切成單字元，無法還原。\
                     把它拉寬一點就會恢復；這只影響終端備援，hook 取得的回覆不受影響。）"
                )
            } else {
                reply
            }),
        }
    };

    // CAS claim + reply in one transaction, so a completed turn never lacks its reply.
    let mut tx = app.db.begin().await?;
    let res = sqlx::query("UPDATE turns SET status='completed_fallback', completed_at=? WHERE id=? AND status='in_flight'")
        .bind(db::now())
        .bind(&turn.id)
        .execute(&mut *tx)
        .await?;
    if res.rows_affected() == 0 {
        return Ok(false);
    }
    tracing::info!(turn = %turn.id, "terminal fallback engaged");

    if let Some(hit) = codex_hit {
        sqlx::query("UPDATE turns SET status='failed', completed_at=? WHERE id=?")
            .bind(db::now())
            .bind(&turn.id)
            .execute(&mut *tx)
            .await?;
        let message = insert_message_tx(
            &mut tx,
            &turn.conversation_id,
            Some(&turn.id),
            "system",
            &hit,
            "system",
            false,
            Some(&read.text),
        )
        .await?;
        remember_pane_cursor_tx(&mut tx, run_id, &read).await?;
        tx.commit().await?;
        emit_message_added(app, &bot.id, message).await;
        let host = db::bot_host(&app.db, &bot.id).await.unwrap_or_else(|_| LOCAL_HOST.to_string());
        apply_codex_limit_hit_quota(app, &host, bot.identity.as_deref(), &hit).await;
        emit_turn(app, &turn.id).await;
        return Ok(true);
    }

    let Some(reply) = reply else {
        // Only our own prompt: the turn is closed (composer unlocks); storing the echo would put words in the agent's mouth.
        tracing::info!(turn = %turn.id, "terminal fallback saw only our own prompt; storing no reply");
        tx.commit().await?;
        remember_pane_cursor(app, run_id, &read).await?;
        emit_turn(app, &turn.id).await;
        return Ok(true);
    };

    let message = insert_message_tx(
        &mut tx,
        &turn.conversation_id,
        Some(&turn.id),
        "assistant",
        &reply,
        "terminal_fallback",
        true,
        Some(&read.text),
    )
    .await?;
    remember_pane_cursor_tx(&mut tx, run_id, &read).await?;
    tx.commit().await?;
    emit_message_added(app, &bot.id, message).await;
    emit_turn(app, &turn.id).await;
    Ok(true)
}

/// Spinner glyphs before an in-progress verb (`✢ Baking…`, `⠦ Thinking… 52s`); braille is codex/grok.
pub(crate) fn is_spinner_glyph(c: char) -> bool {
    crate::capture::claude::is_spinner_glyph(c)
}

/// Mid-turn: a spinner still on screen. codex's `• Working (4s • esc to interrupt)` has no ellipsis,
/// so it's matched separately (2026-09-08).
pub(crate) fn pane_still_busy(screen: &str) -> bool {
    crate::capture::claude::PARSER.still_busy(screen) || screen.lines().any(is_codex_working_line)
}

fn is_codex_working_line(s: &str) -> bool {
    let s = s.trim();
    s.starts_with('•') && s.contains("Working (") && s.contains("esc to interrupt")
}

pub(crate) fn is_tool_progress(reply: &str) -> bool {
    crate::capture::claude::is_tool_progress(reply)
}


/// Adopted run without hooks: no `user_prompt` / `stop` payload will ever come, so the terminal
/// snapshot is the only source (e.g. `managed_by='child'` bots whose parent started the pane).
fn is_hookless(bot: &db::Bot, run: &db::Run) -> bool {
    run.adopted != 0 && bot.inject_hooks == 0
}

/// Cap on a hookless scraped reply: near 200 lines of scrollback it's a screen, not a message.
const HOOKLESS_REPLY_MAX: usize = 6000;

/// Delay before reading an adopted pane: its banner and `agent_status` are still settling.
const ADOPTED_CAPTURE_DELAY: Duration = Duration::from_secs(2);

/// Store a finished hookless exchange as its own `external` turn. Adopted panes are often picked
/// up mid-answer, so `working -> idle` arrives with no turn in flight and the reply was dropped.
/// `seed` (adoption-time capture) only writes into an empty conversation, since re-adoption
/// happens on every restart / reconnect. Caller holds the bot lock; returns whether stored.
async fn capture_hookless_turn_locked(app: &Arc<App>, run_id: &str, seed: bool) -> anyhow::Result<bool> {
    let Some(run) = db::run(&app.db, run_id).await? else { return Ok(false) };
    let Some(bot) = db::bot(&app.db, &run.bot_id).await? else { return Ok(false) };
    if !is_hookless(&bot, &run) || run.state != "running" {
        return Ok(false);
    }
    // Like §4.3: a modal waiting for an answer is not an ended turn.
    if run.agent_status == "blocked" {
        return Ok(false);
    }
    // An in-flight turn belongs to `try_fallback`; capturing too would store it twice.
    if db::in_flight_turn(&app.db, run_id).await?.is_some() {
        return Ok(false);
    }
    let Some(pane_id) = run.pane_id.clone() else { return Ok(false) };
    let conv = db::conversation_id(&app.db, &run.bot_id).await?;
    if seed && conversation_message_count(app, &conv).await? > 0 {
        return Ok(false);
    }
    let client = app
        .herdr_for_run(&run)
        .await
        .ok_or_else(|| anyhow::anyhow!("no Herdr session is available for run `{}`", run.id))?;
    let read = client.pane_read(&pane_id, "recent_unwrapped", 200).await?;
    let fresh = slice_after_cursor(&read.text, run.last_read_tail_hash.as_deref());
    // The pane echo is the only record of what was typed.
    let echo = last_prompt_echo_text(&bot.kind, &fresh);
    // Without a cursor, only the echo marks where the last turn began; with neither, just
    // remember the cursor — storing older turns as one message is worse than nothing.
    if run.last_read_tail_hash.is_none() && echo.is_none() {
        remember_pane_cursor(app, run_id, &read).await?;
        return Ok(false);
    }
    let scraped = extract_reply(&bot.kind, &fresh).or_else(|| clean_screen(&bot.kind, &fresh));
    let reply = match (&echo, scraped) {
        (Some(p), Some(r)) => strip_echoed_prompt(&r, p),
        (None, Some(r)) => r,
        (_, None) => String::new(),
    };
    // Unlike `try_fallback`, no turn waits to be closed, so no 「（終端沒有可辨識的回覆）」 bubble.
    if reply.trim().is_empty() || is_shredded(&reply) {
        remember_pane_cursor(app, run_id, &read).await?;
        return Ok(false);
    }
    // herdr reports `working -> idle` more than once per answer: an identical reply is the same reply.
    if last_assistant_content(app, &conv).await.as_deref().map(str::trim) == Some(reply.trim()) {
        remember_pane_cursor(app, run_id, &read).await?;
        return Ok(false);
    }
    let reply = truncate_hookless_reply(reply);

    let tid = db::ulid();
    let now = db::now();
    sqlx::query(
        "INSERT INTO turns (id, conversation_id, run_id, origin, status, delivery, created_at, completed_at)
         VALUES (?,?,?,'external','completed_fallback','ok',?,?)",
    )
    .bind(&tid)
    .bind(&conv)
    .bind(run_id)
    .bind(&now)
    .bind(&now)
    .execute(&app.db)
    .await?;
    if let Some(text) = echo.as_deref() {
        insert_message(app, &conv, Some(&tid), "user", text, "terminal_fallback", false, None).await?;
        // ULIDs only order by millisecond; wait so the reply doesn't sort above its prompt.
        tokio::time::sleep(Duration::from_millis(2)).await;
    }
    insert_message(app, &conv, Some(&tid), "assistant", &reply, "terminal_fallback", true, Some(&read.text)).await?;
    remember_pane_cursor(app, run_id, &read).await?;
    emit_turn(app, &tid).await;
    tracing::info!(run = %run_id, turn = %tid, bot = %bot.name, seed, "hookless turn captured from the pane");
    Ok(true)
}

/// [`capture_hookless_turn_locked`] for a caller that does not already hold the bot lock.
async fn capture_hookless_turn(app: &Arc<App>, run_id: &str, bot_id: &str, seed: bool) -> anyhow::Result<bool> {
    let lock = app.bot_lock(bot_id).await;
    let _g = lock.lock().await;
    capture_hookless_turn_locked(app, run_id, seed).await
}

/// Adopted hookless run (called by `reconcile`, off its task since both take the bot lock):
/// still `working` → open the `external` turn now; already `idle` → store the exchange once (`seed`).
pub fn spawn_adopted_capture(app: &Arc<App>, run_id: &str, bot_id: &str) {
    let (app, run_id, bot_id) = (app.clone(), run_id.to_string(), bot_id.to_string());
    tokio::spawn(async move {
        tokio::time::sleep(ADOPTED_CAPTURE_DELAY).await;
        let (Ok(Some(run)), Ok(Some(bot))) = (db::run(&app.db, &run_id).await, db::bot(&app.db, &bot_id).await) else {
            return;
        };
        if !is_hookless(&bot, &run) {
            return;
        }
        if run.agent_status == "working" {
            begin_external_turn(&app, &run).await;
            return;
        }
        // `blocked` / `unknown` are refused inside the capture: neither is a finished turn.
        if let Err(e) = capture_hookless_turn(&app, &run_id, &bot_id, true).await {
            tracing::debug!(run = %run_id, error = ?e, "adopted pane capture failed");
        }
    });
}

fn truncate_hookless_reply(reply: String) -> String {
    if reply.chars().count() <= HOOKLESS_REPLY_MAX {
        return reply;
    }
    let mut cut: String = reply.chars().take(HOOKLESS_REPLY_MAX).collect::<String>().trim_end().to_string();
    cut.push_str("\n（終端擷取到此截斷）");
    cut
}

#[cfg(test)]
mod abandon_tests {
    use super::*;
    use crate::testing as tt;

    async fn completed_turn(status: &str) -> (tt::Env, String, String) {
        let env = tt::env().await;
        let app = env.app.clone();
        let bot_id = db::ulid();
        sqlx::query(
            "INSERT INTO bots (id, project_id, name, kind, args_json, autostart, inject_hooks, hook_token, created_at)
             VALUES (?,?,'abandon-test','claude','[]',0,1,'tok',?)",
        )
        .bind(&bot_id)
        .bind(&env.project_id)
        .bind(db::now())
        .execute(&app.db)
        .await
        .unwrap();
        let conversation_id = db::conversation_id(&app.db, &bot_id).await.unwrap();
        let turn_id = db::ulid();
        sqlx::query(
            "INSERT INTO turns (id, conversation_id, origin, status, delivery, created_at, completed_at)
             VALUES (?,?,'web',?,'unknown',?,?)",
        )
        .bind(&turn_id)
        .bind(&conversation_id)
        .bind(status)
        .bind(db::now())
        .bind(db::now())
        .execute(&app.db)
        .await
        .unwrap();
        insert_message(&app, &conversation_id, Some(&turn_id), "assistant", "already complete", "hook", false, None)
            .await
            .unwrap();
        (env, turn_id, conversation_id)
    }

    #[tokio::test]
    async fn abandon_of_a_completed_unknown_delivery_turn_is_a_noop_conflict() {
        for status in ["completed", "completed_fallback"] {
            let (env, turn_id, conversation_id) = completed_turn(status).await;
            let app = env.app.clone();
            let before = sqlx::query_as::<_, db::Turn>("SELECT * FROM turns WHERE id=?")
                .bind(&turn_id)
                .fetch_one(&app.db)
                .await
                .unwrap();
            let before_messages = sqlx::query_as::<_, (String, String, String, String)>(
                "SELECT id, role, content, source FROM messages WHERE conversation_id=? ORDER BY id",
            )
            .bind(&conversation_id)
            .fetch_all(&app.db)
            .await
            .unwrap();

            let err = abandon_turn(&app, &turn_id).await.expect_err("completed turns cannot be abandoned");
            match err {
                LcError::Conflict(body) => {
                    assert_eq!(body["reason"], "turn is neither in-flight nor of unknown delivery");
                    assert_eq!(body["turn_id"], turn_id);
                }
                other => panic!("expected conflict, got {other:?}"),
            }

            let after = sqlx::query_as::<_, db::Turn>("SELECT * FROM turns WHERE id=?")
                .bind(&turn_id)
                .fetch_one(&app.db)
                .await
                .unwrap();
            assert_eq!(after.status, before.status);
            assert_eq!(after.delivery, before.delivery);
            assert_eq!(after.completed_at, before.completed_at);
            let after_messages = sqlx::query_as::<_, (String, String, String, String)>(
                "SELECT id, role, content, source FROM messages WHERE conversation_id=? ORDER BY id",
            )
            .bind(&conversation_id)
            .fetch_all(&app.db)
            .await
            .unwrap();
            assert_eq!(after_messages, before_messages);
        }
    }

    #[tokio::test]
    async fn a_completed_unknown_delivery_turn_does_not_block_a_new_prompt() {
        let env = tt::env().await;
        let app = env.app.clone();
        let bot_id = db::ulid();
        sqlx::query(
            "INSERT INTO bots (id, project_id, name, kind, args_json, autostart, inject_hooks, hook_token, created_at)
             VALUES (?,?,'prompt-unknown','codex','[]',0,1,'tok',?)",
        )
        .bind(&bot_id)
        .bind(&env.project_id)
        .bind(db::now())
        .execute(&app.db)
        .await
        .unwrap();
        let conversation_id = db::conversation_id(&app.db, &bot_id).await.unwrap();
        let run_id = db::ulid();
        sqlx::query(
            "INSERT INTO runs (id, bot_id, state, agent_status, workspace_id, pane_id, agent_name, herdr_session, started_at)
             VALUES (?,?,'running','idle','ws-1','pane-1','agent','test',?)",
        )
        .bind(&run_id)
        .bind(&bot_id)
        .bind(db::now())
        .execute(&app.db)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO turns (id, conversation_id, run_id, origin, status, delivery, created_at, completed_at)
             VALUES (?,?,?,'web','completed','unknown',?,?)",
        )
        .bind(db::ulid())
        .bind(&conversation_id)
        .bind(&run_id)
        .bind(db::now())
        .bind(db::now())
        .execute(&app.db)
        .await
        .unwrap();

        env.herdr.set_agent("agent", "pane-1", true);
        let out = prompt_grouped(&app, &bot_id, "next", "request-1", None, None, &[], None)
            .await
            .expect("a completed unknown-delivery row must not block the next prompt");
        assert_eq!(out.delivery, "unknown", "the mock RPC was reached and failed delivery, rather than the stale row blocking it");
    }
}

#[cfg(test)]
mod hookless_capture_tests {
    //! 對話 for a hookless child run (`managed_by='child'`): scraped off a mock herdr pane via
    //! `capture_hookless_turn_locked`.
    use super::*;
    use crate::testing as tt;

    /// A finished claude exchange as `recent_unwrapped` renders it.
    const EXCHANGE: &str = "\
❯ 幫我看一下 lifecycle.rs
  ⎿  Read lifecycle.rs (4308 lines)
⏺ 看完了：try_fallback 只認 in-flight turn。

✻ Worked for 9s · done 11:35 PM
────────────────────────────────────────────
❯
────────────────────────────────────────────
  ⏵⏵ bypass permissions on (shift+tab to cycle)
";

    struct Child {
        env: tt::Env,
        bot_id: String,
        conv: String,
        run_id: String,
    }

    /// An adopted hookless bot on `pane-1`; `hooks` / `adopted` decide whether the terminal is the source.
    async fn child(status: &str, screen: &str, hooks: i64, adopted: i64) -> Child {
        let env = tt::env().await;
        let app = env.app.clone();
        let bot_id = db::ulid();
        sqlx::query(
            "INSERT INTO bots (id, project_id, name, kind, args_json, autostart, inject_hooks, managed_by, hook_token, created_at)
             VALUES (?,?,'lastq','claude','[]',0,?,'child','tok',?)",
        )
        .bind(&bot_id)
        .bind(&env.project_id)
        .bind(hooks)
        .bind(db::now())
        .execute(&app.db)
        .await
        .unwrap();
        let conv = db::conversation_id(&app.db, &bot_id).await.unwrap();
        let run_id = db::ulid();
        sqlx::query(
            "INSERT INTO runs (id, bot_id, state, agent_status, workspace_id, pane_id, tab_id, adopted, agent_name, herdr_session, started_at)
             VALUES (?,?,'running',?,'ws-1','pane-1','tab-1',?,'parent-lastq','test',?)",
        )
        .bind(&run_id)
        .bind(&bot_id)
        .bind(status)
        .bind(adopted)
        .bind(db::now())
        .execute(&app.db)
        .await
        .unwrap();
        env.herdr.set_screen("pane-1", screen);
        Child { env, bot_id, conv, run_id }
    }

    async fn messages(app: &Arc<App>, conv: &str) -> Vec<(String, String, String)> {
        sqlx::query_as::<_, (String, String, String)>(
            // Same order as `GET /api/bots/{id}/messages`.
            "SELECT role, content, source FROM messages WHERE conversation_id = ? ORDER BY id",
        )
        .bind(conv)
        .fetch_all(&app.db)
        .await
        .unwrap()
    }

    /// The bug: a child adopted mid-answer hits `working -> idle` with no turn in flight, and
    /// `try_fallback` bails — 對話 stayed empty. The edge now becomes a turn of its own.
    #[tokio::test]
    async fn a_finished_exchange_on_the_pane_becomes_a_turn() {
        let c = child("idle", EXCHANGE, 0, 1).await;
        let app = c.env.app.clone();

        assert!(capture_hookless_turn_locked(&app, &c.run_id, false).await.unwrap(), "the screen holds an exchange");

        let msgs = messages(&app, &c.conv).await;
        assert_eq!(msgs.len(), 2, "one prompt, one reply: {msgs:?}");
        assert_eq!(msgs[0].0, "user");
        assert_eq!(msgs[0].1, "幫我看一下 lifecycle.rs", "what was typed straight into the pane");
        assert_eq!(msgs[0].2, "terminal_fallback");
        assert_eq!(msgs[1].0, "assistant");
        assert_eq!(
            msgs[1].1, "看完了：try_fallback 只認 in-flight turn。",
            "the `⏺` reply, with the status line and the composer chrome dropped",
        );
        assert_eq!(msgs[1].2, "terminal_fallback");
        let t = sqlx::query_as::<_, db::Turn>("SELECT * FROM turns WHERE conversation_id = ?")
            .bind(&c.conv)
            .fetch_one(&app.db)
            .await
            .unwrap();
        assert_eq!((t.origin.as_str(), t.status.as_str()), ("external", "completed_fallback"));
        assert_eq!(t.run_id.as_deref(), Some(c.run_id.as_str()));
        assert!(t.completed_at.is_some(), "nothing is left in flight, so the composer is not locked");
    }

    /// Repeated `working -> idle` edges and re-adoption must not grow the conversation.
    #[tokio::test]
    async fn the_same_screen_is_never_stored_twice() {
        let c = child("idle", EXCHANGE, 0, 1).await;
        let app = c.env.app.clone();

        assert!(capture_hookless_turn_locked(&app, &c.run_id, false).await.unwrap());
        assert!(!capture_hookless_turn_locked(&app, &c.run_id, false).await.unwrap(), "nothing new on the pane");
        assert!(!capture_hookless_turn_locked(&app, &c.run_id, true).await.unwrap(), "and the adoption seed is one-shot");

        assert_eq!(messages(&app, &c.conv).await.len(), 2);
    }

    /// The next prompt typed into the pane is a second turn, not an addition to the first.
    #[tokio::test]
    async fn the_next_exchange_is_its_own_turn() {
        let c = child("idle", EXCHANGE, 0, 1).await;
        let app = c.env.app.clone();
        assert!(capture_hookless_turn_locked(&app, &c.run_id, false).await.unwrap());

        c.env.herdr.set_screen("pane-1", &format!("{EXCHANGE}❯ 再看一次\n⏺ 修好了。\n"));
        assert!(capture_hookless_turn_locked(&app, &c.run_id, false).await.unwrap());

        let msgs = messages(&app, &c.conv).await;
        assert_eq!(msgs.len(), 4, "{msgs:?}");
        assert_eq!(msgs[2].1, "再看一次");
        assert_eq!(msgs[3].1, "修好了。");
        let turns: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM turns WHERE conversation_id = ?")
            .bind(&c.conv)
            .fetch_one(&app.db)
            .await
            .unwrap();
        assert_eq!(turns, 2);
    }

    /// A pane we started keeps hooks as source of truth (SPEC §4.3: snapshot is the 備援); this path must not touch it.
    #[tokio::test]
    async fn a_run_with_hooks_is_left_to_its_hooks() {
        let c = child("idle", EXCHANGE, 1, 1).await;
        let app = c.env.app.clone();
        assert!(!capture_hookless_turn_locked(&app, &c.run_id, false).await.unwrap());
        assert!(messages(&app, &c.conv).await.is_empty());

        // Same for a run the daemon started itself, hooks or not.
        let own = child("idle", EXCHANGE, 0, 0).await;
        let app = own.env.app.clone();
        assert!(!capture_hookless_turn_locked(&app, &own.run_id, false).await.unwrap());
        assert!(messages(&app, &own.conv).await.is_empty());
    }

    /// No cursor and no echo: storing the whole scrollback would merge turns, so store nothing and remember the cursor.
    #[tokio::test]
    async fn a_screen_with_no_prompt_echo_is_not_guessed_at() {
        let c = child("idle", "⏺ 一段沒有頭的舊輸出\n", 0, 1).await;
        let app = c.env.app.clone();

        assert!(!capture_hookless_turn_locked(&app, &c.run_id, false).await.unwrap());
        assert!(messages(&app, &c.conv).await.is_empty());
        assert!(
            db::run(&app.db, &c.run_id).await.unwrap().unwrap().last_read_tail_hash.is_some(),
            "the cursor moved, so the next real exchange is read from here",
        );
    }

    /// The adoption seed only fires into an empty conversation (re-adoption on every restart / reconnect).
    #[tokio::test]
    async fn the_adoption_seed_refuses_a_conversation_that_already_has_messages() {
        let c = child("idle", EXCHANGE, 0, 1).await;
        let app = c.env.app.clone();
        insert_message(&app, &c.conv, None, "user", "早先的訊息", "web", false, None).await.unwrap();

        assert!(!capture_hookless_turn_locked(&app, &c.run_id, true).await.unwrap());
        assert_eq!(messages(&app, &c.conv).await.len(), 1);
        let _ = &c.bot_id;
    }
}

#[cfg(test)]
mod issue_17_tests {
    use super::*;
    use crate::testing as tt;

    const FALLBACK_SCREEN: &str = "❯ Reply with PONG\n⏺ PONG\n✻ Worked for 5s · done 1:07 AM\n──────\n❯\n";

    struct Fixture {
        env: tt::Env,
        bot_id: String,
        conversation_id: String,
        run_id: String,
        turn_id: String,
    }

    async fn fixture(kind: &str, screen: &str) -> Fixture {
        let env = tt::env().await;
        let bot_id = db::ulid();
        sqlx::query("INSERT INTO bots (id, project_id, name, kind, hook_token, created_at) VALUES (?,?,?, ?, 'tok', ?)")
            .bind(&bot_id)
            .bind(&env.project_id)
            .bind("issue-17")
            .bind(kind)
            .bind(db::now())
            .execute(&env.app.db)
            .await
            .unwrap();
        let conversation_id = db::conversation_id(&env.app.db, &bot_id).await.unwrap();
        let run_id = db::ulid();
        sqlx::query(
            "INSERT INTO runs (id, bot_id, state, agent_status, pane_id, herdr_session, started_at)
             VALUES (?,?,'running','idle','pane-17','test',?)",
        )
        .bind(&run_id)
        .bind(&bot_id)
        .bind(db::now())
        .execute(&env.app.db)
        .await
        .unwrap();
        let turn_id = db::ulid();
        sqlx::query(
            "INSERT INTO turns (id, conversation_id, run_id, origin, status, delivery, created_at)
             VALUES (?,?,?,'web','in_flight','ok',?)",
        )
        .bind(&turn_id)
        .bind(&conversation_id)
        .bind(&run_id)
        .bind(db::now())
        .execute(&env.app.db)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO messages (id, conversation_id, turn_id, role, content, source, created_at)
             VALUES (?,?,?,'user','Reply with PONG','web',?)",
        )
        .bind(db::ulid())
        .bind(&conversation_id)
        .bind(&turn_id)
        .bind(db::now())
        .execute(&env.app.db)
        .await
        .unwrap();
        env.herdr.set_screen("pane-17", screen);
        Fixture { env, bot_id, conversation_id, run_id, turn_id }
    }

    async fn event_kinds(mut rx: tokio::sync::broadcast::Receiver<crate::state::WsEvent>) -> Vec<String> {
        let mut kinds = Vec::new();
        while kinds.len() < 2 {
            kinds.push(tokio::time::timeout(Duration::from_secs(1), rx.recv()).await.unwrap().unwrap().kind);
        }
        kinds
    }

    async fn turn(app: &Arc<App>, id: &str) -> db::Turn {
        sqlx::query_as::<_, db::Turn>("SELECT * FROM turns WHERE id=?")
            .bind(id)
            .fetch_one(&app.db)
            .await
            .unwrap()
    }

    fn count(f: &Fixture, method: &str) -> usize {
        f.env.herdr.methods().iter().filter(|m| *m == method).count()
    }

    async fn run_and_bot(f: &Fixture) -> (db::Run, db::Bot) {
        let app = &f.env.app;
        (db::run(&app.db, &f.run_id).await.unwrap().unwrap(), db::bot(&app.db, &f.bot_id).await.unwrap().unwrap())
    }

    fn live(f: &Fixture, pane: crate::testing::LivePane) {
        f.env.herdr.live_pane("pane-17", pane);
    }

    fn wide() -> crate::testing::LivePane {
        crate::testing::LivePane { width: Some(120), ..Default::default() }
    }

    fn not(reason: &'static str, retry: bool) -> Delivered {
        Delivered::NotAttempted { reason, retry }
    }

    /// A claude run bound to a session whose transcript the live pane writes on Enter.
    async fn with_transcript(f: &Fixture, pane: crate::testing::LivePane) -> std::path::PathBuf {
        let t = f.env.dir.join(format!("session-{}.jsonl", db::ulid()));
        std::fs::write(&t, "").unwrap();
        sqlx::query("UPDATE runs SET native_session_id = 'sess-1', transcript_path = ? WHERE id = ?")
            .bind(t.to_str().unwrap())
            .bind(&f.run_id)
            .execute(&f.env.app.db)
            .await
            .unwrap();
        live(f, crate::testing::LivePane { transcript_file: Some(t.clone()), ..pane });
        t
    }

    #[tokio::test]
    async fn a_short_line_is_proven_by_its_echo_row_on_a_pane_of_known_width() {
        let f = fixture("claude", "").await;
        live(&f, crate::testing::LivePane { transcript: vec!["⏺ 先前的回覆".into()], ..wide() });
        let app = f.env.app.clone();
        db::set_pane_typed(&app.db, &f.run_id).await.unwrap();
        let (run, bot) = run_and_bot(&f).await;
        let client = client_for_run(&app, &run).await.unwrap();

        let text = "加一個功能除了按 SKU 之外";
        assert_eq!(deliver_prompt(&app, &client, &run, &bot, text, false, false).await.unwrap(), Delivered::Submitted);
        let pane = f.env.herdr.pane("pane-17").unwrap();
        assert!(pane.composer.is_empty());
        assert_eq!(pane.transcript.iter().filter(|l| l.contains("SKU")).count(), 1, "只送一次");
        assert_eq!(count(&f, "pane.send_text"), 1);
    }

    /// 極窄 pane＋很長的單行：有 transcript 就用 transcript 證明，不會因為畫面折行而永遠 unknown（第七輪 #3）。
    #[tokio::test]
    async fn a_long_line_on_a_narrow_pane_is_proven_by_the_transcript() {
        let f = fixture("claude", "").await;
        with_transcript(&f, crate::testing::LivePane { width: Some(20), ..Default::default() }).await;
        let app = f.env.app.clone();
        db::set_pane_typed(&app.db, &f.run_id).await.unwrap();
        let (run, bot) = run_and_bot(&f).await;
        let client = client_for_run(&app, &run).await.unwrap();

        let text = "please rewrite the offline quote importer so that it is at least three times faster";
        assert_eq!(deliver_prompt(&app, &client, &run, &bot, text, false, false).await.unwrap(), Delivered::Submitted);
        assert_eq!(count(&f, "pane.send_text"), 1);
    }

    #[tokio::test]
    async fn a_multi_line_prompt_is_proven_by_the_transcript_and_sent_once() {
        let f = fixture("claude", "").await;
        with_transcript(&f, wide()).await;
        let app = f.env.app.clone();
        db::set_pane_typed(&app.db, &f.run_id).await.unwrap();
        let (run, bot) = run_and_bot(&f).await;
        let client = client_for_run(&app, &run).await.unwrap();

        let text = "第一行：先看報告\n第二行：再改程式\n第三行：最後回報";
        assert_eq!(deliver_prompt(&app, &client, &run, &bot, text, false, false).await.unwrap(), Delivered::Submitted);
        let pane = f.env.herdr.pane("pane-17").unwrap();
        assert_eq!(pane.transcript.iter().filter(|l| l.starts_with('❯')).count(), 1);
        assert_eq!(count(&f, "pane.send_text"), 1);
    }

    /// 規劃完、打字前證據檔被刪掉：一個字都沒打，回 NotAttempted(transcript_unreadable)（sol 第十一輪）。
    #[tokio::test]
    async fn an_evidence_file_removed_after_planning_is_not_attempted() {
        let f = fixture("claude", "").await;
        let t = with_transcript(&f, wide()).await;
        let app = f.env.app.clone();
        db::set_pane_typed(&app.db, &f.run_id).await.unwrap();
        let (run, bot) = run_and_bot(&f).await;
        let client = client_for_run(&app, &run).await.unwrap();
        let plan = plan_delivery(&app, &client, &run, &bot, "第一行\n第二行", false, false).await.unwrap().unwrap();
        std::fs::remove_file(&t).unwrap();
        let out = execute_delivery(&app, &client, &run, &bot, "第一行\n第二行", plan).await.unwrap();
        assert_eq!(out, not("transcript_unreadable", true));
        assert_eq!(count(&f, "pane.send_text") + count(&f, "pane.send_keys"), 0);
    }

    /// herdr 收了 `format: ansi` 卻回 `format: text`：照樣用，但會（節流地）警告一次。
    #[tokio::test]
    async fn a_plain_answer_to_a_styled_read_is_warned_about() {
        let f = fixture("claude", "").await;
        live(&f, wide());
        f.env.herdr.ignore_ansi.store(true, std::sync::atomic::Ordering::SeqCst);
        let app = f.env.app.clone();
        db::set_pane_typed(&app.db, &f.run_id).await.unwrap();
        let (run, bot) = run_and_bot(&f).await;
        let client = client_for_run(&app, &run).await.unwrap();
        assert_eq!(deliver_prompt(&app, &client, &run, &bot, "Reply with PONG please", false, false).await.unwrap(), Delivered::Submitted);
        assert!(!should_warn_plain_for_ansi("pane-17"), "the read already used this pane's warning");
    }

    /// session 在送出途中換掉：transcript 證據不再適用，回 Unproven（不是 Submitted、也不是沒送）。
    #[tokio::test]
    async fn a_session_change_mid_delivery_is_unproven() {
        let f = fixture("claude", "").await;
        with_transcript(&f, wide()).await;
        let app = f.env.app.clone();
        db::set_pane_typed(&app.db, &f.run_id).await.unwrap();
        let (run, bot) = run_and_bot(&f).await;
        let client = client_for_run(&app, &run).await.unwrap();
        let text = "第一行\n第二行";
        let plan = plan_delivery(&app, &client, &run, &bot, text, false, false).await.unwrap().unwrap();
        sqlx::query("UPDATE runs SET native_session_id = 'sess-2' WHERE id = ?").bind(&f.run_id).execute(&app.db).await.unwrap();
        assert_eq!(execute_delivery(&app, &client, &run, &bot, text, plan).await.unwrap(), Delivered::Unproven("session_changed"));
    }

    /// 沒有無損證據（grok 多行）：照樣打字送出一次，回 Unverified。
    #[tokio::test]
    async fn a_prompt_with_no_lossless_proof_is_typed_once_and_unverified() {
        let f = fixture("grok", "").await;
        live(&f, crate::testing::LivePane { boxed: true, ..wide() });
        let app = f.env.app.clone();
        db::set_pane_typed(&app.db, &f.run_id).await.unwrap();
        let (run, bot) = run_and_bot(&f).await;
        let client = client_for_run(&app, &run).await.unwrap();

        let out = deliver_prompt(&app, &client, &run, &bot, "第一行\n第二行", false, false).await.unwrap();
        assert_eq!(out, Delivered::Unverified);
        assert_eq!(count(&f, "pane.send_text"), 1);
        let pane = f.env.herdr.pane("pane-17").unwrap();
        assert_eq!(pane.transcript.iter().filter(|l| l.starts_with('❯')).count(), 1);
    }

    /// 沒有證據的那條路，TUI 晚一幀才把貼上的字畫出來：多等一個 settle 再看，照常按 Enter 送出，
    /// 不是判 `nothing_typed` 把字留在框裡（review2 deliv 上一輪 #2 的副作用）。只貼一次。
    #[tokio::test]
    async fn a_late_redraw_on_an_unverified_run_is_still_submitted_not_left_in_the_box() {
        let f = fixture("grok", "").await;
        // 第一次讀的時候框還是空的：字「晚一幀」才出現。
        live(&f, crate::testing::LivePane { boxed: true, swallow_text: true, ..wide() });
        let app = f.env.app.clone();
        db::set_pane_typed(&app.db, &f.run_id).await.unwrap();
        let (run, bot) = run_and_bot(&f).await;
        let client = client_for_run(&app, &run).await.unwrap();
        let (calls, panes) = (f.env.herdr.calls.clone(), f.env.herdr.live.clone());
        let redraw = tokio::spawn(async move {
            // 貼上之後的第一次讀畫面過去了，才把字畫進框裡。
            loop {
                let m: Vec<String> = calls.lock().unwrap().iter().map(|(m, _)| m.clone()).collect();
                if let Some(pos) = m.iter().position(|x| x == "pane.send_text") {
                    if m[pos..].iter().any(|x| x == "pane.read") {
                        break;
                    }
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            let mut live = panes.lock().unwrap();
            let p = live.get_mut("pane-17").unwrap();
            p.swallow_text = false;
            p.composer = vec!["第一行".into(), "第二行".into()];
        });
        let out = deliver_prompt(&app, &client, &run, &bot, "第一行\n第二行", false, false).await.unwrap();
        redraw.await.unwrap();
        assert_eq!(out, Delivered::Unverified, "晚一幀的字照樣送出");
        assert_eq!(count(&f, "pane.send_text"), 1, "沒有證據就不重貼");
        assert!(count(&f, "pane.send_keys") >= 1, "有按 Enter");
        assert!(f.env.herdr.pane("pane-17").unwrap().composer.is_empty(), "字沒有留在框裡");
    }

    /// 打過字、證不明的 turn 絕不自動重送（`auto_resend = 0`）：它很可能已經被收下了。
    #[tokio::test]
    async fn a_turn_marked_no_auto_resend_is_never_resent() {
        let f = fixture("grok", "").await;
        live(&f, crate::testing::LivePane { boxed: true, ..wide() });
        let app = f.env.app.clone();
        sqlx::query("UPDATE turns SET delivery_verified = 0, auto_resend = 0 WHERE id = ?")
            .bind(&f.turn_id)
            .execute(&app.db)
            .await
            .unwrap();
        let sent = vec!["Reply with PONG".to_string()];
        assert!(!resend_lost_prompt(&app, &f.run_id, &f.turn_id, &sent).await.sent());
        assert_eq!(count(&f, "pane.send_text") + count(&f, "pane.send_keys"), 0);
    }

    /// 有無損證據證明已經送進 session 的 turn，畫面上找不到回音（長段貼上被摺起來）也不自動重送（review3 c3 M4）。
    #[tokio::test]
    async fn a_delivery_proven_by_evidence_is_never_resent_even_if_the_screen_lost_its_echo() {
        let f = fixture("claude", "").await;
        live(&f, crate::testing::LivePane { transcript: vec!["❯ [Pasted text #1 +20 lines]".into()], ..wide() });
        let app = f.env.app.clone();
        db::set_pane_typed(&app.db, &f.run_id).await.unwrap();
        mark_delivery(&app, &f.turn_id, Delivered::Submitted.record().unwrap()).await;

        let sent = vec!["Reply with PONG".to_string()];
        assert!(prompt_never_reached_screen("claude", &f.env.herdr.pane("pane-17").unwrap().render(), &sent), "畫面上真的找不到");
        assert!(!resend_lost_prompt(&app, &f.run_id, &f.turn_id, &sent).await.sent());
        assert_eq!(count(&f, "pane.send_text") + count(&f, "pane.send_keys"), 0, "有證據就不再打一次");
    }

    /// 沒有證據不等於不能重送（AGM 2026-09-16）：`agent.prompt` 那條路記成未驗證，但畫面證明它沒進去時照樣重送。
    #[tokio::test]
    async fn an_unverified_but_resendable_turn_is_still_resent() {
        let f = fixture("grok", "").await;
        live(&f, crate::testing::LivePane { boxed: true, ..wide() });
        let app = f.env.app.clone();
        sqlx::query("UPDATE turns SET delivery_verified = 0, auto_resend = 1 WHERE id = ?")
            .bind(&f.turn_id)
            .execute(&app.db)
            .await
            .unwrap();
        let sent = vec!["Reply with PONG".to_string()];
        assert!(resend_lost_prompt(&app, &f.run_id, &f.turn_id, &sent).await.sent());
        assert_eq!(count(&f, "pane.send_text"), 1);
    }

    /// codex 本機：找到它自己的 rollout 就用 rollout 逐字證明多行 prompt。
    #[tokio::test]
    async fn a_codex_multi_line_prompt_is_proven_by_its_rollout() {
        let f = fixture("codex", "").await;
        let app = f.env.app.clone();
        let home = f.env.dir.join("codex-home");
        let day = home.join("sessions/2026/09/14");
        std::fs::create_dir_all(&day).unwrap();
        let log = day.join("rollout-2026-09-14T10-00-00-sess-codex.jsonl");
        std::fs::write(&log, "").unwrap();
        sqlx::query("UPDATE bots SET env_json = ? WHERE id = ?")
            .bind(json!({"CODEX_HOME": home.to_str().unwrap()}).to_string())
            .bind(&f.bot_id)
            .execute(&app.db)
            .await
            .unwrap();
        sqlx::query("UPDATE runs SET native_session_id = 'sess-codex' WHERE id = ?").bind(&f.run_id).execute(&app.db).await.unwrap();
        // codex 的 composer／回音 marker 是 `› `；mock pane 畫的是 `❯`，所以這裡只看 rollout 證據本身。
        let (run, bot) = run_and_bot(&f).await;
        let client = client_for_run(&app, &run).await.unwrap();
        let pane_cols = client.pane_size("pane-17").await.ok().flatten().map(|(w, _)| w);
        let inputs = ProofInputs {
            kind: &bot.kind,
            host_is_local: true,
            hooks: true,
            session_id: run.native_session_id.as_deref(),
            transcript_path: None,
            codex_log: codex_home(&app, &bot).await.and_then(|h| codex_session_log(&h, "sess-codex")),
            waited_for_log: false,
            pane_cols,
        };
        assert_eq!(
            choose_proof(&inputs, "第一行\n第二行"),
            Ok(Proof::Transcript { format: LogFormat::Codex, path: std::fs::canonicalize(&log).unwrap(), session_id: "sess-codex".into() }),
        );
    }

    /// Enter 被吃掉，spinner 一直在重畫：不能判成功；已經按過鍵，所以是 Unproven。
    #[tokio::test]
    async fn a_swallowed_enter_is_unproven_even_while_the_spinner_redraws() {
        let f = fixture("claude", "").await;
        live(&f, crate::testing::LivePane { swallow_enter: true, ..wide() });
        let app = f.env.app.clone();
        db::set_pane_typed(&app.db, &f.run_id).await.unwrap();
        let (run, bot) = run_and_bot(&f).await;
        let client = client_for_run(&app, &run).await.unwrap();

        assert_eq!(deliver_prompt(&app, &client, &run, &bot, "go", false, false).await.unwrap(), Delivered::Unproven("still_in_box"));
        assert!(f.env.herdr.pane("pane-17").unwrap().transcript.is_empty());
    }

    #[tokio::test]
    async fn a_swallowed_paste_is_pasted_once_more_and_then_unproven() {
        let f = fixture("claude", "").await;
        live(&f, crate::testing::LivePane { swallow_text: true, ..wide() });
        let app = f.env.app.clone();
        db::set_pane_typed(&app.db, &f.run_id).await.unwrap();
        let (run, bot) = run_and_bot(&f).await;
        let client = client_for_run(&app, &run).await.unwrap();

        let out = deliver_prompt(&app, &client, &run, &bot, "Reply with PONG please", false, false).await.unwrap();
        assert_eq!(out, Delivered::Unproven("nothing_typed"));
        assert_eq!(count(&f, "pane.send_text"), 2);
        assert_eq!(count(&f, "pane.send_keys"), 0);
    }

    /// 框裡有任何東西（含只有空白、空白第二行、行尾空白草稿、建議句）都不代送，而且零寫入（第七輪 #1）。
    #[tokio::test]
    async fn anything_in_the_box_is_left_alone_with_zero_writes() {
        let cases: Vec<(Vec<String>, &str)> = vec![
            (vec!["我自己在打的草稿".into()], "草稿"),
            (vec![" ".into()], "只有一個空白（marker 後多一格）"),
            (vec!["".into(), "".into()], "空白第二行"),
            (vec!["draft with trailing spaces  ".into()], "行尾空白草稿"),
            (vec!["Reply with PONG please".into()], "跟要送的一模一樣"),
        ];
        for (composer, why) in cases {
            let f = fixture("claude", "").await;
            live(&f, crate::testing::LivePane { composer: composer.clone(), ..wide() });
            let app = f.env.app.clone();
            db::set_pane_typed(&app.db, &f.run_id).await.unwrap();
            let (run, bot) = run_and_bot(&f).await;
            let client = client_for_run(&app, &run).await.unwrap();

            let out = deliver_prompt(&app, &client, &run, &bot, "Reply with PONG please", false, false).await.unwrap();
            assert_eq!(out, not("composer_busy", true), "{why}");
            assert_eq!(count(&f, "pane.send_text") + count(&f, "pane.send_keys"), 0, "{why}: 零寫入");
            assert_eq!(f.env.herdr.pane("pane-17").unwrap().composer, composer, "{why}: 框原封不動");
        }
    }

    /// 輸入框只有 dim 的「建議下一句」（2026-09-14 k8bw2f 的交辦卡 queued）：是空框，照樣送出，建議句不會被一起送。
    #[tokio::test]
    async fn a_dim_suggested_prompt_is_an_empty_box_and_the_prompt_goes_through() {
        let f = fixture("claude", "").await;
        live(&f, crate::testing::LivePane { suggestion: Some("把 4b 和 4c 補做完".into()), ..wide() });
        let app = f.env.app.clone();
        db::set_pane_typed(&app.db, &f.run_id).await.unwrap();
        let (run, bot) = run_and_bot(&f).await;
        let client = client_for_run(&app, &run).await.unwrap();
        let out = deliver_prompt(&app, &client, &run, &bot, "Reply with PONG please", false, false).await.unwrap();
        assert_ne!(out, not("composer_busy", true));
        assert!(!matches!(out, Delivered::NotAttempted { .. }), "{out:?}");
        assert_eq!(count(&f, "pane.send_text"), 1);
        let pane = f.env.herdr.pane("pane-17").unwrap();
        assert!(pane.transcript.iter().any(|l| l.contains("Reply with PONG please")), "{:?}", pane.transcript);
        assert!(!pane.transcript.iter().any(|l| l.contains("補做完")));
    }

    /// 使用者真的打了跟佔位字一字不差的草稿（mock 讀不到 dim）：不能當空框，零寫入（sol 第九輪 #2）。
    #[tokio::test]
    async fn a_draft_identical_to_the_placeholder_is_left_alone() {
        let f = fixture("claude", "").await;
        let draft = vec!["Try \"fix lint errors\"".to_string()];
        live(&f, crate::testing::LivePane { composer: draft.clone(), ..wide() });
        let app = f.env.app.clone();
        db::set_pane_typed(&app.db, &f.run_id).await.unwrap();
        let (run, bot) = run_and_bot(&f).await;
        let client = client_for_run(&app, &run).await.unwrap();
        let out = deliver_prompt(&app, &client, &run, &bot, "Reply with PONG please", false, false).await.unwrap();
        assert_eq!(out, not("composer_busy", true));
        assert_eq!(count(&f, "pane.send_text") + count(&f, "pane.send_keys"), 0);
        assert_eq!(f.env.herdr.pane("pane-17").unwrap().composer, draft);
    }

    #[tokio::test]
    async fn an_agent_without_a_session_binding_is_typed_into_not_prompted() {
        let f = fixture("claude", "").await;
        live(&f, wide());
        f.env.herdr.set_agent("issue-17", "pane-17", false);
        let app = f.env.app.clone();
        assert!(!db::pane_typed(&app.db, &f.run_id).await.unwrap());
        let (run, bot) = run_and_bot(&f).await;
        let client = client_for_run(&app, &run).await.unwrap();

        assert_eq!(deliver_prompt(&app, &client, &run, &bot, "Reply with PONG please", false, false).await.unwrap(), Delivered::Submitted);
        assert_eq!(count(&f, "agent.prompt"), 0);
        assert!(db::pane_typed(&app.db, &f.run_id).await.unwrap());
    }

    #[tokio::test]
    async fn an_agent_with_a_session_binding_still_goes_through_agent_prompt() {
        let f = fixture("claude", "").await;
        live(&f, wide());
        f.env.herdr.set_agent("issue-17", "pane-17", true);
        let app = f.env.app.clone();
        let (run, bot) = run_and_bot(&f).await;
        let client = client_for_run(&app, &run).await.unwrap();
        let _ = deliver_prompt(&app, &client, &run, &bot, "Reply with PONG please", false, false).await;
        assert_eq!(count(&f, "agent.prompt"), 1);
        assert_eq!(count(&f, "pane.send_text"), 0);
    }

    /// `agent.prompt` 走完之後 DB 記的是「沒有證據、但可以重送」，而且沒有用掉重送額度。
    #[tokio::test]
    async fn the_agent_prompt_route_is_recorded_unverified_but_still_resendable() {
        // 路由本身由 `an_agent_with_a_session_binding_still_goes_through_agent_prompt` 蓋；
        // mock 不實作 agent.prompt，這裡只驗那條路的結果怎麼記。
        let f = fixture("claude", "").await;
        let app = f.env.app.clone();
        crate::lifecycle::prompt::mark_delivery(&app, &f.turn_id, Delivered::Handed.record().unwrap()).await;
        let (delivery, verified, auto, resends): (String, i64, i64, i64) =
            sqlx::query_as("SELECT delivery, delivery_verified, auto_resend, resend_count FROM turns WHERE id = ?")
                .bind(&f.turn_id)
                .fetch_one(&app.db)
                .await
                .unwrap();
        assert_eq!((delivery.as_str(), verified, auto), ("ok", 0, 1), "沒有證據，但重送照舊");
        assert_eq!(resends, 0, "沒有用掉重送額度");
    }

    /// 打字證不明（grok 多行）：一樣沒有證據，但重送關掉、額度也被用掉（回滾到舊 binary 也不會重打）。
    #[tokio::test]
    async fn a_typed_but_unprovable_prompt_is_recorded_without_auto_resend() {
        let f = fixture("grok", "").await;
        let app = f.env.app.clone();
        crate::lifecycle::prompt::mark_delivery(&app, &f.turn_id, Delivered::Unverified.record().unwrap()).await;
        let (delivery, verified, auto, resends): (String, i64, i64, i64) =
            sqlx::query_as("SELECT delivery, delivery_verified, auto_resend, resend_count FROM turns WHERE id = ?")
                .bind(&f.turn_id)
                .fetch_one(&app.db)
                .await
                .unwrap();
        assert_eq!((delivery.as_str(), verified, auto), ("ok", 0, 0));
        assert_eq!(resends, crate::lifecycle::MAX_PROMPT_RESENDS);
    }

    /// 升級前就存在的列：`auto_resend` 預設 1，行為與今天相同——verified=1 的照樣可重送，
    /// 舊的 unverified 列因為當時已把 resend_count 頂到上限，還是不會被重送。
    #[tokio::test]
    async fn rows_written_before_the_split_keep_todays_behaviour() {
        let f = fixture("grok", "").await;
        live(&f, crate::testing::LivePane { boxed: true, ..wide() });
        let app = f.env.app.clone();
        let auto: i64 = sqlx::query_scalar("SELECT auto_resend FROM turns WHERE id = ?")
            .bind(&f.turn_id)
            .fetch_one(&app.db)
            .await
            .unwrap();
        assert_eq!(auto, 1, "既有列的預設");
        sqlx::query("UPDATE turns SET delivery_verified = 0, resend_count = ? WHERE id = ?")
            .bind(crate::lifecycle::MAX_PROMPT_RESENDS)
            .bind(&f.turn_id)
            .execute(&app.db)
            .await
            .unwrap();
        let sent = vec!["Reply with PONG".to_string()];
        assert!(!resend_lost_prompt(&app, &f.run_id, &f.turn_id, &sent).await.sent(), "舊的 unverified 列照舊不重送");
        assert_eq!(count(&f, "pane.send_text"), 0);
    }

    /// 打字前讀不到畫面：一個字都沒打，是「稍後再試」，不是錯誤也不是 unknown（sol 第十輪 #1）。
    #[tokio::test]
    async fn a_pane_that_cannot_be_read_before_typing_is_not_attempted() {
        let f = fixture("claude", "__READ_ERROR__").await;
        let app = f.env.app.clone();
        db::set_pane_typed(&app.db, &f.run_id).await.unwrap();
        let (run, bot) = run_and_bot(&f).await;
        let client = client_for_run(&app, &run).await.unwrap();
        let t = f.env.dir.join("t.jsonl");
        std::fs::write(&t, "").unwrap();
        let run = db::Run { native_session_id: Some("s".into()), transcript_path: Some(t.to_str().unwrap().into()), ..run };
        let out = deliver_prompt(&app, &client, &run, &bot, "Reply with PONG please", false, false).await.unwrap();
        assert_eq!(out, not("composer_unreadable", true));
        assert_eq!(count(&f, "pane.send_text"), 0);
    }

    /// herdr 不認得 `format: ansi`：退回純文字讀法照樣送；純文字讀法下佔位字自然當非空。
    #[tokio::test]
    async fn a_herdr_without_styled_reads_falls_back_to_plain_reads() {
        let f = fixture("claude", "").await;
        live(&f, wide());
        f.env.herdr.reject_ansi.store(true, std::sync::atomic::Ordering::SeqCst);
        let app = f.env.app.clone();
        db::set_pane_typed(&app.db, &f.run_id).await.unwrap();
        let (run, bot) = run_and_bot(&f).await;
        let client = client_for_run(&app, &run).await.unwrap();
        assert_eq!(deliver_prompt(&app, &client, &run, &bot, "Reply with PONG please", false, false).await.unwrap(), Delivered::Submitted);
        assert_eq!(count(&f, "pane.send_text"), 1);

        let f = fixture("claude", "").await;
        live(&f, crate::testing::LivePane { composer: vec!["Try \"fix lint errors\"".into()], ..wide() });
        f.env.herdr.reject_ansi.store(true, std::sync::atomic::Ordering::SeqCst);
        let app = f.env.app.clone();
        db::set_pane_typed(&app.db, &f.run_id).await.unwrap();
        let (run, bot) = run_and_bot(&f).await;
        let client = client_for_run(&app, &run).await.unwrap();
        assert_eq!(deliver_prompt(&app, &client, &run, &bot, "Reply with PONG please", false, false).await.unwrap(), not("composer_busy", true));
        assert_eq!(count(&f, "pane.send_text"), 0);
    }

    /// `runs.pane_typed` 寫不進去時一個字都還沒打：可重試的 NotAttempted，不是 `Err`（→ unknown，review3 c4 L5）。
    #[tokio::test]
    async fn nothing_is_typed_when_the_pane_typed_marker_cannot_be_stored() {
        let f = fixture("claude", "").await;
        live(&f, wide());
        let app = f.env.app.clone();
        db::set_pane_typed(&app.db, &f.run_id).await.unwrap();
        let (run, bot) = run_and_bot(&f).await;
        let client = client_for_run(&app, &run).await.unwrap();
        sqlx::query("ALTER TABLE runs RENAME TO runs_real").execute(&app.db).await.unwrap();
        sqlx::query("CREATE VIEW runs AS SELECT * FROM runs_real").execute(&app.db).await.unwrap();

        assert_eq!(
            deliver_prompt(&app, &client, &run, &bot, "Reply with PONG please", false, false).await.unwrap(),
            not("pane_typed_unwritable", true),
        );
        assert_eq!(count(&f, "pane.send_text"), 0);
        assert_eq!(count(&f, "agent.prompt"), 0);
    }

    #[tokio::test]
    async fn the_in_process_memo_keeps_a_pane_on_the_typing_path() {
        let f = fixture("claude", "").await;
        live(&f, wide());
        f.env.herdr.set_agent("issue-17", "pane-17", true);
        let app = f.env.app.clone();
        crate::lifecycle::remember_pane_typed(&f.run_id);
        let (run, bot) = run_and_bot(&f).await;
        let client = client_for_run(&app, &run).await.unwrap();
        assert_eq!(deliver_prompt(&app, &client, &run, &bot, "Reply with PONG please", false, false).await.unwrap(), Delivered::Submitted);
        assert_eq!(count(&f, "agent.prompt"), 0);
    }

    #[tokio::test]
    async fn an_unreadable_pane_typed_marker_falls_back_to_typing_not_to_agent_prompt() {
        let f = fixture("claude", "").await;
        live(&f, wide());
        f.env.herdr.set_agent("issue-17", "pane-17", true);
        let app = f.env.app.clone();
        sqlx::query("UPDATE runs SET pane_typed = 'broken' WHERE id = ?").bind(&f.run_id).execute(&app.db).await.unwrap();
        let (run, bot) = run_and_bot(&f).await;
        let client = client_for_run(&app, &run).await.unwrap();
        assert_eq!(deliver_prompt(&app, &client, &run, &bot, "Reply with PONG please", false, false).await.unwrap(), Delivered::Submitted);
        assert_eq!(count(&f, "agent.prompt"), 0);
    }

    #[tokio::test]
    async fn no_pane_is_not_attempted_and_never_an_agent_prompt() {
        let f = fixture("claude", "").await;
        let app = f.env.app.clone();
        sqlx::query("UPDATE runs SET pane_id = NULL WHERE id = ?").bind(&f.run_id).execute(&app.db).await.unwrap();
        let (run, bot) = run_and_bot(&f).await;
        let client = client_for_run(&app, &run).await.unwrap();
        let out = deliver_prompt(&app, &client, &run, &bot, "Reply with PONG please", false, false).await.unwrap();
        assert_eq!(out, not("no_pane_to_type_into", true));
        assert_eq!(count(&f, "agent.prompt"), 0);
    }

    #[tokio::test]
    async fn two_concurrent_resends_send_the_prompt_exactly_once() {
        let f = fixture("claude", "").await;
        live(&f, crate::testing::LivePane { transcript: vec!["⏺ 先前的回覆".into()], ..wide() });
        let app = f.env.app.clone();
        db::set_pane_typed(&app.db, &f.run_id).await.unwrap();
        let sent = vec!["Reply with PONG".to_string()];
        let (a, b) = tokio::join!(
            resend_lost_prompt(&app, &f.run_id, &f.turn_id, &sent),
            resend_lost_prompt(&app, &f.run_id, &f.turn_id, &sent),
        );
        assert_eq!([a, b].iter().filter(|x| x.sent()).count(), 1);
        let pane = f.env.herdr.pane("pane-17").unwrap();
        assert_eq!(pane.transcript.iter().filter(|l| l.contains("Reply with PONG")).count(), 1);
        assert!(!db::claim_resend(&app.db, &f.turn_id, MAX_PROMPT_RESENDS).await);
    }

    /// 群組訊息的泡泡帶著 @mention，實際送給 bot 的是去掉 mention 的字（`turns.prompt_text`）。
    async fn group_turn(f: &Fixture, bubble: &str, delivered: &str) {
        let app = &f.env.app;
        sqlx::query("UPDATE messages SET content = ? WHERE turn_id = ? AND role = 'user'")
            .bind(bubble)
            .bind(&f.turn_id)
            .execute(&app.db)
            .await
            .unwrap();
        sqlx::query("UPDATE turns SET prompt_text = ? WHERE id = ?").bind(delivered).bind(&f.turn_id).execute(&app.db).await.unwrap();
    }

    /// review3 c3 M4：畫面上已經有實際送出的那句（沒有 @mention）時，不能因為拿泡泡原文去搜找不到就重送。
    #[tokio::test]
    async fn a_group_prompt_on_screen_without_its_mentions_is_not_resent() {
        let f = fixture("claude", "").await;
        live(&f, crate::testing::LivePane { transcript: vec!["❯ 請跑一次完整的測試並回報".into()], ..wide() });
        let app = f.env.app.clone();
        db::set_pane_typed(&app.db, &f.run_id).await.unwrap();
        group_turn(&f, "@AM-2-M @AM-3-X 請跑一次完整的測試並回報", "請跑一次完整的測試並回報").await;

        let sent = turn_echo_texts(&app, &f.turn_id).await;
        assert_eq!(sent.first().map(String::as_str), Some("請跑一次完整的測試並回報"), "實際送出的字排第一：{sent:?}");
        assert_eq!(resend_lost_prompt(&app, &f.run_id, &f.turn_id, &sent).await, Resend::Skipped);
        assert_eq!(count(&f, "pane.send_text"), 0, "已經到了，不重送");
    }

    /// 真的沒到時，重送打的是當初實際送出的字，不是帶著路由語法的泡泡原文。
    #[tokio::test]
    async fn a_lost_group_prompt_is_resent_as_delivered_without_its_mentions() {
        let f = fixture("claude", "").await;
        live(&f, crate::testing::LivePane { transcript: vec!["⏺ 先前的回覆".into()], ..wide() });
        let app = f.env.app.clone();
        db::set_pane_typed(&app.db, &f.run_id).await.unwrap();
        group_turn(&f, "@AM-2-M @AM-3-X 請跑一次完整的測試並回報", "請跑一次完整的測試並回報").await;

        let sent = turn_echo_texts(&app, &f.turn_id).await;
        assert!(resend_lost_prompt(&app, &f.run_id, &f.turn_id, &sent).await.sent());
        let pane = f.env.herdr.pane("pane-17").unwrap();
        assert!(pane.transcript.iter().any(|l| l == "❯ 請跑一次完整的測試並回報"), "{:?}", pane.transcript);
        assert!(!pane.transcript.iter().any(|l| l.contains("@AM-2-M")), "路由語法不能打進 pane：{:?}", pane.transcript);
    }

    /// 重送在打字前被擋下（證據檔一時讀不到）：額度退回，watchdog 隔一下用退回的額度再試一次，這次送出去
    /// （review2 deliv #4：以前退回的額度沒人用得到，回合馬上被判失敗）。
    #[cfg(unix)]
    #[tokio::test]
    async fn a_resend_blocked_before_typing_is_retried_with_the_refunded_budget() {
        use std::os::unix::fs::PermissionsExt;
        let f = fixture("claude", "").await;
        let t = with_transcript(&f, wide()).await;
        let app = f.env.app.clone();
        db::set_pane_typed(&app.db, &f.run_id).await.unwrap();
        let generation = 4242;
        app.stall_timers.lock().await.insert(f.run_id.clone(), generation);
        let sent = vec!["Reply with PONG please".to_string()];
        std::fs::set_permissions(&t, std::fs::Permissions::from_mode(0o000)).unwrap();
        let unlock = {
            let t = t.clone();
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_millis(300)).await;
                std::fs::set_permissions(&t, std::fs::Permissions::from_mode(0o644)).unwrap();
            })
        };
        let out = resend_with_retry(&app, &f.run_id, &f.bot_id, &f.turn_id, &sent, generation, Duration::from_millis(900)).await;
        unlock.await.unwrap();
        assert_eq!(out, Some((true, None)), "第二次用退回的額度送出去");
        assert_eq!(count(&f, "pane.send_text"), 1, "只送了一次");
        assert!(!db::claim_resend(&app.db, &f.turn_id, MAX_PROMPT_RESENDS).await, "額度這次真的用掉了");
    }

    /// 兩次都被擋：回合照樣判失敗，但訊息說出試過重送、被什麼擋下，不是「agent 沒反應」。
    #[cfg(unix)]
    #[tokio::test]
    async fn a_resend_blocked_twice_says_what_blocked_it() {
        use std::os::unix::fs::PermissionsExt;
        let f = fixture("claude", "").await;
        let t = with_transcript(&f, wide()).await;
        let app = f.env.app.clone();
        db::set_pane_typed(&app.db, &f.run_id).await.unwrap();
        app.stall_timers.lock().await.insert(f.run_id.clone(), 7);
        let sent = vec!["Reply with PONG please".to_string()];
        std::fs::set_permissions(&t, std::fs::Permissions::from_mode(0o000)).unwrap();
        let out = resend_with_retry(&app, &f.run_id, &f.bot_id, &f.turn_id, &sent, 7, Duration::from_millis(50)).await;
        std::fs::set_permissions(&t, std::fs::Permissions::from_mode(0o644)).unwrap();
        let Some((false, Some(why))) = out else { panic!("expected blocked twice, got {out:?}") };
        assert_eq!(count(&f, "pane.send_text"), 0, "一個字都沒打");
        assert!(db::claim_resend(&app.db, &f.turn_id, MAX_PROMPT_RESENDS).await, "額度還在（兩次都退回了）");

        fail_stalled_turn(&app, &f.run_id, &f.bot_id, &f.turn_id, false, false, Some(why)).await.unwrap();
        let msg: String = sqlx::query_scalar("SELECT content FROM messages WHERE turn_id=? AND role='system'")
            .bind(&f.turn_id)
            .fetch_one(&app.db)
            .await
            .unwrap();
        assert!(msg.contains("試著自動重送時被擋下") && msg.contains(why), "{msg}");

        // 計時器被換掉（新的回合、取消）就停手，不動任何東西。
        assert_eq!(resend_with_retry(&app, &f.run_id, &f.bot_id, &f.turn_id, &sent, 8, Duration::from_millis(1)).await, None);
    }

    #[tokio::test]
    async fn stall_commits_system_message_before_turn_updated() {
        let f = fixture("claude", "not logged in\n").await;
        let app = f.env.app.clone();
        let rx = app.subscribe();

        fail_stalled_turn(&app, &f.run_id, &f.bot_id, &f.turn_id, false, false, None).await.unwrap();
        let message: (String, String, String) = sqlx::query_as(
            "SELECT role, content, source FROM messages WHERE conversation_id=? AND turn_id=? AND role='system'",
        )
        .bind(&f.conversation_id)
        .bind(&f.turn_id)
        .fetch_one(&app.db)
        .await
        .unwrap();
        assert_eq!(message.0, "system");
        assert!(message.1.contains("not logged in"));
        assert_eq!(message.2, "system");
        assert_eq!(turn(&app, &f.turn_id).await.status, "failed");
        let kinds = event_kinds(rx).await;
        assert_eq!(kinds, vec!["message_added", "turn_updated"]);
    }
}

#[cfg(test)]
mod progress_rate_tests {
    use super::{progress_due, PROGRESS_MIN_GAP};
    use std::time::{Duration, Instant};

    #[test]
    fn the_gap_is_the_documented_four_frames_a_second() {
        assert_eq!(PROGRESS_MIN_GAP * 4, Duration::from_secs(1));
    }

    #[test]
    fn a_run_that_has_not_emitted_yet_goes_out_at_once() {
        assert!(progress_due(None, false));
    }

    #[test]
    fn a_frame_inside_the_window_is_held_back() {
        assert!(!progress_due(Some(&Instant::now()), false));
    }

    #[test]
    fn the_window_opens_again_once_the_gap_has_passed() {
        let last = Instant::now() - PROGRESS_MIN_GAP - Duration::from_millis(1);
        assert!(progress_due(Some(&last), false));
    }

    /// The poller's last frame must not stay in `pending` when the turn ends inside the window (`force`).
    #[test]
    fn force_ignores_the_budget() {
        assert!(progress_due(Some(&Instant::now()), true));
    }
}


#[cfg(test)]
mod lost_prompt_tests {
    use super::prompt_never_reached_screen;

    /// 2026-09-14 w1HJ:pH（wits-c1-op-xh）turn 01M2F9DRR09TBHHYZA211ZRDHY 被判 stall 時的真實畫面：
    /// `/effort max` 的回饋加上空輸入列，使用者的「think more if can be even faster」一個字都沒有。
    const EFFORT_MAX_LOST: &str = "  Any screen that mounts many MUI inputs at once will hit this. If other pages also stall,
  the same default can go into the app-wide theme. Everything is still uncommitted: this fix,
  the round-2 performance changes, and the iStore multi-page parser fix.

✻ Sautéed for 15m 9s · done 2:18 PM

❯ /effort max
  ⎿  Set effort level to max (this session only): Maximum capability with deepest reasoning.
     May use excessive tokens resulting in long response times or overthinking. Use sparingly
     for the hardest tasks.
                                                                              615330 tokens
─────────────────────────────────────────────────────────────────────────────────────────────
❯
─────────────────────────────────────────────────────────────────────────────────────────────
  user. | web | OP5 61% | 5h:53%(rst 2h 35m) | 7d:95%(rst 6d 21h) | F5:100%
  ⏵⏵ bypass permissions on (shift+tab to cycle) · ← for agents
";

    fn sent(s: &str) -> Vec<String> {
        vec![s.to_string()]
    }

    /// claude 現行框（兩條全寬框線夾著 `❯`）與它的 `Try "…"` 佔位字：都是空框（sol 第八輪 #2）。
    #[test]
    fn the_current_claude_frame_and_its_placeholder_are_empty() {
        use crate::lifecycle::{box_state, BoxState};
        assert_eq!(box_state("claude", EFFORT_MAX_LOST), BoxState::Empty, "w1HJ:pH 的真實空框");
        // 實機 idle claude 的輸入列是 `❯` 加一個不斷行空白（ANSI 讀法還帶 `\r`）。
        let nbsp = EFFORT_MAX_LOST.replacen("\n❯\n", "\n❯\u{a0}\r\n", 1);
        assert_eq!(box_state("claude", &nbsp), BoxState::Empty);
        // 佔位字只有畫成 dim 才算；純文字的 `Try "…"` 可能是有人打的。
        let plain = EFFORT_MAX_LOST.replacen("\n❯\n", "\n❯ Try \"refactor <filepath>\"\n", 1);
        assert_eq!(box_state("claude", &plain), BoxState::NonEmpty);
        let dim = EFFORT_MAX_LOST.replacen("\n❯\n", "\n❯\u{a0}\u{1b}[2mTry \"refactor <filepath>\"\u{1b}[22m\n", 1);
        assert_eq!(box_state("claude", &dim), BoxState::Empty);
        let typed = EFFORT_MAX_LOST.replacen("\n❯\n", "\n❯ Try \"refactor\" please\n", 1);
        assert_eq!(box_state("claude", &typed), BoxState::NonEmpty);
        let one_space = EFFORT_MAX_LOST.replacen("\n❯\n", "\n❯  \n", 1);
        assert_eq!(box_state("claude", &one_space), BoxState::NonEmpty, "使用者真的打了一格");
    }

    #[test]
    fn the_effort_max_incident_is_a_prompt_that_never_arrived() {
        assert!(prompt_never_reached_screen("claude", EFFORT_MAX_LOST, &sent("think more if can be even faster")));
    }

    #[test]
    fn a_submitted_prompt_is_not_resent() {
        // claude 收下了：`❯ …` 印在對話區、輸入列又空了。重送就是重複派工。
        let accepted = EFFORT_MAX_LOST.replacen(
            "─────────────────────────────────────────────────────────────────────────────────────────────\n❯\n",
            "❯ think more if can be even faster\n─────────────────────────────────────────────────────────────────────────────────────────────\n❯\n",
            1,
        );
        assert!(!prompt_never_reached_screen("claude", &accepted, &sent("think more if can be even faster")));
        // 排隊中的訊息（`  ❯ …`）一樣算到了。
        let queued = EFFORT_MAX_LOST.replacen("615330 tokens", "615330 tokens\n  ❯ think more if can be even faster", 1);
        assert!(!prompt_never_reached_screen("claude", &queued, &sent("think more if can be even faster")));
    }

    #[test]
    fn text_still_in_the_box_is_the_enter_nudges_job_not_a_resend() {
        let in_box = EFFORT_MAX_LOST.replacen("\n❯\n", "\n❯ think more if can be even faster\n", 1);
        assert!(!prompt_never_reached_screen("claude", &in_box, &sent("think more if can be even faster")));
    }

    #[test]
    fn a_prompt_too_short_to_look_for_is_never_resent() {
        // 兩個字的 prompt 在任何畫面都找得到／找不到都不可靠：證明不了「沒到」就不重送。
        assert!(!prompt_never_reached_screen("claude", EFFORT_MAX_LOST, &sent("go")));
        assert!(!prompt_never_reached_screen("claude", EFFORT_MAX_LOST, &[]));
    }

    #[test]
    fn a_long_prompt_matches_on_its_head_even_when_wrapped() {
        // 長訊息在畫面上會折行；比對去掉空白後的開頭，折在哪裡都一樣。
        let long = "please make the offline quote import even faster and report the numbers again";
        let wrapped = EFFORT_MAX_LOST.replacen("615330 tokens", "615330 tokens\n❯ please make the offline\n  quote import even faster", 1);
        assert!(!prompt_never_reached_screen("claude", &wrapped, &sent(long)));
        assert!(prompt_never_reached_screen("claude", EFFORT_MAX_LOST, &sent(long)));
    }
}
