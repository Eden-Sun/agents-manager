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

/// 輪詢讀 DB 失敗時的退避上限（#193）。讀不到不等於回合收掉了：poller 是這一回合終端側的安全網，一次讀錯就
/// 退出、Stop hook 又剛好漏掉的話，那一回合就再也沒人收。所以讀不到只延後、重讀，間隔從一個輪詢間隔加倍到這裡
/// 為止（DB 長時間壞掉時不每 700ms 打一行 log）；只有讀到了、確定不再是這一回合才退出。
const PROGRESS_BACKOFF_MAX: Duration = Duration::from_secs(30);

fn progress_backoff(interval: Duration, failures: u32) -> Duration {
    interval.saturating_mul(1u32 << failures.min(6)).min(PROGRESS_BACKOFF_MAX.max(interval))
}

#[cfg(not(test))]
fn progress_interval(_run_id: &str) -> Duration {
    PROGRESS_INTERVAL
}

/// 測試可以把單一 run 的輪詢調快（[`progress_poll_tests`]），別的測試的 poller 照舊。
#[cfg(test)]
fn progress_interval(run_id: &str) -> Duration {
    progress_poll_tests::interval(run_id).unwrap_or(PROGRESS_INTERVAL)
}

/// 這一輪還是不是這一回合的 poller：讀到了、確定不是（回合收掉或換了一筆、run 或 bot 沒了）回 `Ok(None)`；
/// 讀不到回錯（#193）。bot 與送了什麼讀到一次就留著。
async fn still_ours(
    app: &Arc<App>,
    run_id: &str,
    bot_id: &str,
    turn_id: &str,
    bot: &mut Option<db::Bot>,
    sent: &mut Option<Vec<String>>,
) -> anyhow::Result<Option<db::Run>> {
    match db::in_flight_turn(&app.db, run_id).await? {
        Some(t) if t.id == turn_id => {}
        _ => return Ok(None),
    }
    let Some(run) = db::run(&app.db, run_id).await? else { return Ok(None) };
    if bot.is_none() {
        let Some(b) = db::bot(&app.db, bot_id).await? else { return Ok(None) };
        *bot = Some(b);
    }
    if sent.is_none() {
        *sent = Some(turn_echo_texts(app, turn_id).await?);
    }
    Ok(Some(run))
}

/// 讀不到：記一筆（第一次 warn，之後 debug），回傳新的連續失敗次數。
fn poll_unreadable(turn_id: &str, failures: u32, e: &anyhow::Error) -> u32 {
    if failures == 0 {
        tracing::warn!(turn = %turn_id, error = %e, "progress poller cannot read the turn's state; retrying, not giving up the turn");
    } else {
        tracing::debug!(turn = %turn_id, failures, error = %e, "progress poller still cannot read the turn's state");
    }
    failures.saturating_add(1)
}

/// While a turn is in flight, poll the pane and push partial replies as `turn_progress`. Also the
/// idle-prompt safety net that calls `try_fallback` (see below). Stops once the turn leaves `in_flight`.
pub async fn arm_progress(app: &Arc<App>, run_id: &str, bot_id: &str, turn_id: &str) {
    // A new turn is starting: whatever cut the *previous* one short is history (§4.3a). 清不掉就在 poller 裡
    // 再清：留著的話，這一回合斷在同一句錯誤上會被「同一則只記一次」吞掉（#193）。
    let mut error_cleared = crate::turn_error::clear(app, run_id, bot_id).await.is_ok();
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
        let interval = progress_interval(&run_id);
        // 連續讀不到幾次（#193）：讀到就歸零。
        let mut failures = 0u32;
        let mut bot: Option<db::Bot> = None;
        // What we sent, to strip the pane's echo off every frame.
        let mut sent: Option<Vec<String>> = None;
        let mut last = (String::new(), String::new(), String::new());
        let mut quiet = 0u32;
        let mut pending: Option<Value> = None;
        let mut grok_scanned = false;
        loop {
            tokio::time::sleep(progress_backoff(interval, failures)).await;
            if started.elapsed() > PROGRESS_MAX {
                break;
            }
            #[cfg(test)]
            super::race_point::hit("progress_poll", &run_id).await;
            // 讀不到（DB 一時 busy／I/O error）不是「回合收掉了」：延後重讀，不放掉這一回合（#193）。
            let run = match still_ours(&app2, &run_id, &bot_id, &turn_id, &mut bot, &mut sent).await {
                Ok(Some(run)) => run,
                Ok(None) => break,
                Err(e) => {
                    failures = poll_unreadable(&turn_id, failures, &e);
                    continue;
                }
            };
            failures = 0;
            // `still_ours` 讀到了才回 `Some`：兩個都已經有了。
            let (Some(bot), Some(sent)) = (bot.as_ref(), sent.as_deref()) else { continue };
            if !error_cleared {
                error_cleared = crate::turn_error::clear(&app2, &run_id, &bot_id).await.is_ok();
            }
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
                Ok(None) => break,
                // 不知道是不是 `blocked`（對話框）：這一輪不算閒著，也不歸零（#193）。以前當成空字串，照樣累計。
                Err(e) => {
                    failures = poll_unreadable(&turn_id, failures, &e);
                    continue;
                }
            };
            // grok 撞週限停在 `blocked`（#222）：沒有 working->idle 那條邊，備援與掃描都不會跑；daemon 重啟時已經停在那裡的
            // 也不會再有 blocked 邊。這裡是安全網，每個 blocked 只掃一次。
            if agent_status == "blocked" && bot.kind == "grok" && !grok_scanned && !grok_limit_notice_lines(&read.text).is_empty() {
                grok_scanned = true;
                let lock = app2.bot_lock(&bot_id).await;
                let _g = lock.lock().await;
                if let Err(e) = capture_codex_usage_notices(&app2, &bot_id, &run_id).await {
                    tracing::debug!(turn = %turn_id, error = ?e, "grok limit screen capture failed; keeping the poller");
                    grok_scanned = false;
                }
            }
            if agent_status != "blocked" {
                grok_scanned = false;
            }
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
                    try_fallback(&app2, &run_id, Some(&turn_id)).await
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
    // claude 自己起頭的回合（背景 shell 的 task notification）畫面上沒有新的回音，最後一個是上一則使用者 prompt，
    // 已經記在上一回合底下——不是這一回合的 user 訊息（issue #224，`transcript_origin`）。
    let by_the_cli = match db::bot(&app.db, &run.bot_id).await {
        Ok(Some(bot)) => super::transcript_origin::started_by_the_cli_itself(&bot.kind, run.transcript_path.as_deref()).await,
        _ => false,
    };
    let echo = if by_the_cli { None } else { pane_prompt_echo(app, &run).await };
    if by_the_cli {
        tracing::info!(turn = %tid, "external turn started by the CLI itself (task notification); no user message");
    }
    if let Some(text) = echo {
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
///
/// 讀不到回錯（#193），不是「什麼都沒送」：空的清單會讓備援把我們自己的 prompt 當成 agent 的回覆存下來、
/// stall watchdog 找不到框裡的字也不重送，接著判失敗。
pub(crate) async fn turn_echo_texts(app: &Arc<App>, turn_id: &str) -> anyhow::Result<Vec<String>> {
    let rows = db::turn_user_messages_with_attachments(&app.db, turn_id).await?;
    let mut out = Vec::new();
    let delivered: Option<String> = sqlx::query_scalar("SELECT prompt_text FROM turns WHERE id = ?")
        .bind(turn_id)
        .fetch_optional(&app.db)
        .await?
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
    Ok(out)
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
    if kind == "claude" && is_our_folded_paste(&box_text, sent) {
        return true;
    }
    let needle: String = squash(sent).chars().take(COMPOSER_HEAD).collect();
    if needle.chars().count() < COMPOSER_HEAD_MIN {
        return false;
    }
    squash(&box_text).contains(&needle)
}

/// claude 把長段貼上摺成 `[Pasted text #3 +10 lines]`，框裡看不到原文，上面的比對永遠落空：
/// 補 Enter 不會發生、重送又因為框不空被擋，12 秒後直接判失敗。2026-09-19 AM-1-XH：子 agent v4
/// 卡在 blocked 的通知就這樣停在父 bot 的輸入框，父 bot 一直沒回應。
///
/// 框裡**只有**一個摺起來的貼上，而且 `+N lines` 的 N 正好是我們送的字的換行數，才算是我們的：
/// 使用者自己貼的東西行數對不上就不按 Enter。
fn is_our_folded_paste(box_text: &str, sent: &str) -> bool {
    let Some(rest) = box_text.trim().strip_prefix("[Pasted text #") else { return false };
    let Some(rest) = rest.strip_suffix(" lines]") else { return false };
    let Some((id, lines)) = rest.split_once(" +") else { return false };
    if id.is_empty() || !id.chars().all(|c| c.is_ascii_digit()) {
        return false;
    }
    let Ok(lines) = lines.parse::<usize>() else { return false };
    lines > 0 && lines == sent.trim_end().matches('\n').count()
}

/// Early check after delivery: enough for the box to draw, short of the full stall deadline.
const NUDGE_EARLY_SECS: u64 = 3;

/// After re-sending Enter, how long the agent gets to react before the turn is failed.
const NUDGE_GRACE_SECS: u64 = 8;

/// 判斷救不救得回來要的狀態讀不到時，stall watchdog 隔多久整輪再看一次（#193）。
const STALL_RECHECK: Duration = Duration::from_secs(10);

/// Press Enter if our prompt is still in the box and the agent idle. Every reason to do nothing is
/// `Ok(false)`. The DB unreadable is an error (#193): whether the text is still in the box is then
/// unknown, and the watchdog must not go on to fail the turn as if it weren't.
async fn nudge_unsent_prompt(app: &Arc<App>, run_id: &str, turn_id: &str, sent: &[String]) -> anyhow::Result<bool> {
    let Some(run) = db::run(&app.db, run_id).await? else { return Ok(false) };
    if run.agent_status == "working" || run.agent_status == "blocked" {
        return Ok(false);
    }
    if !matches!(db::in_flight_turn(&app.db, run_id).await?, Some(t) if t.id == turn_id && t.delivery == "ok") {
        return Ok(false);
    }
    let Some(bot) = db::bot(&app.db, &run.bot_id).await? else { return Ok(false) };
    let Some(pane) = run.pane_id.clone() else { return Ok(false) };
    let Ok(client) = client_for_run(app, &run).await else { return Ok(false) };
    let Ok(read) = client.pane_read(&pane, "visible", 80).await else { return Ok(false) };
    if !sent.iter().any(|p| composer_holds_prompt(&bot.kind, &read.text, p)) {
        return Ok(false);
    }
    if let Err(e) = client.pane_send_keys(&pane, &["Enter"]).await {
        tracing::warn!(error = ?e, run_id, "could not re-send Enter for an unsent prompt");
        return Ok(false);
    }
    tracing::warn!(run_id, turn = %turn_id, "prompt was still in the composer; re-sent Enter");
    Ok(true)
}

/// [`nudge_unsent_prompt`]，送了什麼先讀（讀到一次就留在 `sent`）。讀不到回錯（#193）。
async fn nudge_if_unsent(app: &Arc<App>, run_id: &str, turn_id: &str, sent: &mut Option<Vec<String>>) -> anyhow::Result<bool> {
    if sent.is_none() {
        *sent = Some(turn_echo_texts(app, turn_id).await?);
    }
    nudge_unsent_prompt(app, run_id, turn_id, sent.as_deref().unwrap_or_default()).await
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
    /// 重送按過鍵卻證明不了（`Unproven`，或打字之後出錯）：turn 已改記 `delivery='unknown'`，不能判失敗
    /// （review3 c3 M5）。`fail_stalled_turn` 只收 `delivery='ok'`，所以接下來那一步自然不動它。
    Unproven,
    /// 同上，但 `delivery='unknown'` 寫不進去（#193）：欠著（原因在這裡），補上之前 watchdog 不判失敗——打過的字
    /// 可能已經被收下，判 failed 會讓 AGM 重派同一件事。
    UnprovenOwed(&'static str),
    /// 判斷要不要重送的狀態讀不到（DB，#193）：這一輪不知道，不能當成「不必重送」接著判失敗。
    Unreadable,
}

fn resend_unreadable(turn_id: &str, e: &anyhow::Error) -> Resend {
    tracing::warn!(turn = %turn_id, error = %e, "cannot tell whether a lost prompt should be re-delivered; not failing the turn on it");
    Resend::Unreadable
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
    // DB 讀不到是 `Unreadable`，不是 `Skipped`（#193）：`Skipped` 之後 watchdog 就判失敗了。
    let run = match db::run(&app.db, run_id).await {
        Ok(Some(run)) => run,
        Ok(None) => return Resend::Skipped,
        Err(e) => return resend_unreadable(turn_id, &e),
    };
    if run.agent_status == "working" || run.agent_status == "blocked" {
        return Resend::Skipped;
    }
    // 重送看 `auto_resend`，不看有沒有證據（AGM 2026-09-16）：打過字但證不明的那條路重送會重複派工，
    // 所以它是 0；`agent.prompt` 一樣沒有證據，但它沒送進去才會走到這裡，重送是安全的。
    match db::in_flight_turn(&app.db, run_id).await {
        Ok(Some(t)) if t.id == turn_id && t.delivery == "ok" && t.auto_resend != 0 => {}
        Ok(_) => return Resend::Skipped,
        Err(e) => return resend_unreadable(turn_id, &e),
    }
    let bot = match db::bot(&app.db, &run.bot_id).await {
        Ok(Some(bot)) => bot,
        Ok(None) => return Resend::Skipped,
        Err(e) => return resend_unreadable(turn_id, &e),
    };
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
    match db::claim_resend(&app.db, turn_id, MAX_PROMPT_RESENDS).await {
        Ok(true) => {}
        Ok(false) => return Resend::Skipped,
        Err(e) => return resend_unreadable(turn_id, &e),
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
            unproven(app, &bot.id, turn_id, why).await
        }
        // `execute_delivery` 在打第一個字之前的失敗都是 `NotAttempted`，走到 `Err` 就是鍵可能已經送出去了。
        Err(e) => {
            tracing::warn!(run_id, turn = %turn_id, error = %e, "re-delivering a lost prompt failed after typing started");
            unproven(app, &bot.id, turn_id, "resend_error").await
        }
    }
}

/// 重送打過字、證明不了：記成 `delivery='unknown'`；寫不進去就欠著（`UnprovenOwed`，#193）。
async fn unproven(app: &Arc<App>, bot_id: &str, turn_id: &str, why: &'static str) -> Resend {
    match mark_resend_unproven(app, bot_id, turn_id, why).await {
        Ok(()) => Resend::Unproven,
        Err(e) => {
            tracing::warn!(turn = %turn_id, error = ?e, "could not record an unproven resend as unknown delivery; owed, the turn is not failed meanwhile");
            Resend::UnprovenOwed(why)
        }
    }
}

/// 重送已經打了字卻證明不了：字可能還在框裡（claude compact 時吞 Enter），也可能已經被收下、排在後面。
/// 兩種都不是「agent 沒反應」，判 failed 會讓交辦跟著失敗、AGM 重派同一件事（review3 c3 M5）。
/// 照 §6「Unproven 才會變成 unknown」記成 `delivery='unknown'`，並留一則說明；hook 來了照樣認領，
/// 真的沒人收由 §4.3b 收尾。寫不進去回錯（#193）：呼叫端記成欠著，補上之前不判失敗。
async fn mark_resend_unproven(app: &Arc<App>, bot_id: &str, turn_id: &str, why: &str) -> anyhow::Result<()> {
    let text = format!(
        "（畫面上完全沒有這則訊息，已自動重打一次，但證明不了有送出（{why}）：字可能還留在輸入框裡，也可能已經被收下、排在後面。\
         這一則先記成「送達不明」，不判失敗。請到終端看一下——還在框裡就按 Enter 或清掉；已經在跑就等它回完。）"
    );
    let res = async {
        let mut tx = app.db.begin().await?;
        let moved = sqlx::query(
            "UPDATE turns SET delivery='unknown', delivery_verified=0 WHERE id=? AND status='in_flight' AND delivery='ok'",
        )
        .bind(turn_id)
        .execute(&mut *tx)
        .await?
        .rows_affected();
        if moved == 0 {
            return anyhow::Ok(None);
        }
        let conv: String = sqlx::query_scalar("SELECT conversation_id FROM turns WHERE id=?").bind(turn_id).fetch_one(&mut *tx).await?;
        let m = insert_message_tx(&mut tx, &conv, Some(turn_id), "system", &text, "system", false, None).await?;
        tx.commit().await?;
        anyhow::Ok(Some(m))
    }
    .await?;
    if let Some(m) = res {
        emit_message_added(app, bot_id, m).await;
        emit_turn(app, turn_id).await;
    }
    Ok(())
}

/// The watchdog's resend, with one retry when the first try was refused before typing anything.
///
/// Before this, a refunded budget was never used: `arm_stall` failed the turn right after the refusal,
/// and nothing re-arms a watchdog later (review2 deliv #4). `None` = the stall timer was replaced
/// meanwhile (the caller stops); otherwise `(what the last try came to, what blocked a try)`.
async fn resend_with_retry(
    app: &Arc<App>,
    run_id: &str,
    bot_id: &str,
    turn_id: &str,
    sent: &[String],
    generation: u64,
    retry_after: Duration,
) -> Option<(Resend, Option<&'static str>)> {
    let mut blocked = None;
    let mut last = Resend::Skipped;
    for attempt in 0..2 {
        if attempt > 0 {
            tokio::time::sleep(retry_after).await;
        }
        let lock = app.bot_lock(bot_id).await;
        let _g = lock.lock().await;
        if app.stall_timers.lock().await.get(run_id) != Some(&generation) {
            return None;
        }
        last = resend_lost_prompt(app, run_id, turn_id, sent).await;
        match last {
            Resend::Sent => return Some((last, None)),
            Resend::Blocked { reason, retry } => {
                blocked = Some(reason);
                if !retry {
                    break;
                }
            }
            Resend::Skipped | Resend::Unproven | Resend::UnprovenOwed(_) | Resend::Unreadable => return Some((last, blocked)),
        }
    }
    Some((last, blocked))
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
        let mut st = Stall::default();
    // Every prompt gets the early look: a single line pasted into a busy TUI loses its Enter too.
        tokio::time::sleep(Duration::from_secs(NUDGE_EARLY_SECS)).await;
        {
            let lock = app2.bot_lock(&bot_id).await;
            let _g = lock.lock().await;
            if app2.stall_timers.lock().await.get(&run_id) != Some(&generation) {
                return;
            }
            // 早看的這一眼讀不到就算了：期限那一輪會再看，那一輪讀不到才擋著不判失敗。
            match nudge_if_unsent(&app2, &run_id, &turn_id, &mut st.sent).await {
                Ok(n) => st.nudged = n,
                Err(e) => tracing::debug!(turn = %turn_id, error = %e, "early nudge could not read the turn; the deadline looks again"),
            }
        }
        tokio::time::sleep(Duration::from_secs(STALL_SECS - NUDGE_EARLY_SECS)).await;
        loop {
            match stall_round(&app2, &run_id, &bot_id, &turn_id, generation, &mut st, Duration::from_secs(NUDGE_GRACE_SECS)).await {
                StallRound::Replaced => return,
                StallRound::Unsure => tokio::time::sleep(STALL_RECHECK).await,
                StallRound::Done => break,
            }
        }
        let mut timers = app2.stall_timers.lock().await;
        if timers.get(&run_id) == Some(&generation) {
            timers.remove(&run_id);
        }
    });
}

/// stall watchdog 一輪一輪之間要記得的事。
#[derive(Debug, Default)]
struct Stall {
    /// 送了什麼（[`turn_echo_texts`]）：讀到一次就留著，讀不到下一步再讀（#193）。
    sent: Option<Vec<String>>,
    nudged: bool,
    resent: bool,
    blocked: Option<&'static str>,
    /// 重送打過字、`delivery='unknown'` 卻寫不進去（[`Resend::UnprovenOwed`]）：每一輪先補，補上之前不判失敗。
    owed_unproven: Option<&'static str>,
}

#[derive(Debug, PartialEq, Eq)]
enum StallRound {
    /// 計時器被換掉（新的回合、取消）：停手，什麼都不動。
    Replaced,
    /// 有一步讀不到、判斷不了（#193）：不判失敗，隔 [`STALL_RECHECK`] 整輪再看。
    Unsure,
    Done,
}

fn stall_unsure(turn_id: &str, e: &anyhow::Error) -> bool {
    tracing::warn!(turn = %turn_id, error = %e, "stall watchdog cannot tell whether the prompt can still be saved; not failing the turn yet");
    true
}

/// 期限到了的那一輪：還在框裡就補 Enter，完全沒到就重送一次，都沒用才判失敗。
///
/// 哪一步讀不到（#193）都不當成「不在框裡」「不必重送」接著判失敗——以前送了什麼讀不到就當成什麼都沒送，框裡的字
/// 不補 Enter、沒到的不重送，回合直接判 failed；重送打過字卻記不成 `unknown` 也照樣判 failed，AGM 重派同一件事。
async fn stall_round(
    app: &Arc<App>,
    run_id: &str,
    bot_id: &str,
    turn_id: &str,
    generation: u64,
    st: &mut Stall,
    grace: Duration,
) -> StallRound {
    let mut unsure = false;
    // Deadline: if the text is still there, press Enter and grant a grace period first.
    {
        let lock = app.bot_lock(bot_id).await;
        let _g = lock.lock().await;
        if app.stall_timers.lock().await.get(run_id) != Some(&generation) {
            return StallRound::Replaced;
        }
        match nudge_if_unsent(app, run_id, turn_id, &mut st.sent).await {
            Ok(n) => st.nudged |= n,
            Err(e) => unsure = stall_unsure(turn_id, &e),
        }
    }
    #[cfg(test)]
    super::race_point::hit("stall_after_nudge", turn_id).await;
    // Not in the box either: the prompt never reached the TUI. Deliver it again once instead of
    // failing the turn with a system message the user has to act on.
    let mut resent_now = false;
    if !st.nudged && !st.resent && !unsure {
        // 期限那一次 nudge 讀到了才走到這裡：`sent` 已經有了。
        let sent = st.sent.clone().unwrap_or_default();
        let Some((last, blocked)) =
            resend_with_retry(app, run_id, bot_id, turn_id, &sent, generation, Duration::from_secs(RESEND_RETRY_SECS)).await
        else {
            return StallRound::Replaced;
        };
        st.blocked = blocked;
        match last {
            Resend::Sent => resent_now = true,
            Resend::UnprovenOwed(why) => st.owed_unproven = Some(why),
            Resend::Unreadable => unsure = true,
            Resend::Skipped | Resend::Unproven | Resend::Blocked { .. } => {}
        }
        st.resent = resent_now;
    }
    if st.nudged || resent_now {
        tokio::time::sleep(grace).await;
    }
    // A resend can land in a busy box just like the first try did.
    if resent_now {
        let lock = app.bot_lock(bot_id).await;
        let _g = lock.lock().await;
        if app.stall_timers.lock().await.get(run_id) != Some(&generation) {
            return StallRound::Replaced;
        }
        match nudge_if_unsent(app, run_id, turn_id, &mut st.sent).await {
            Ok(true) => {
                drop(_g);
                tokio::time::sleep(grace).await;
            }
            Ok(false) => {}
            Err(e) => unsure = stall_unsure(turn_id, &e),
        }
    }
    let lock = app.bot_lock(bot_id).await;
    let _g = lock.lock().await;
    if app.stall_timers.lock().await.get(run_id) != Some(&generation) {
        return StallRound::Replaced;
    }
    if let Some(why) = st.owed_unproven {
        match mark_resend_unproven(app, bot_id, turn_id, why).await {
            Ok(()) => st.owed_unproven = None,
            Err(e) => unsure = stall_unsure(turn_id, &e),
        }
    }
    if unsure {
        return StallRound::Unsure;
    }
    if let Err(e) = fail_stalled_turn(app, run_id, bot_id, turn_id, st.nudged, st.resent, st.blocked).await {
        tracing::warn!(error = ?e, "stall watchdog failed");
    }
    StallRound::Done
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
    let res = super::turn_controller::fail_on(&mut tx, turn_id, super::turn_controller::DeliveryOnFail::Keep, "重送用盡").await?;
    if res != super::turn_controller::Outcome::Applied {
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
    let res = super::turn_controller::fail(&app.db, turn_id, super::turn_controller::DeliveryOnFail::FailedIfUnknown, "使用者放棄這一回合")
        .await
        .map_err(up)?;
    if res != super::turn_controller::Outcome::Applied {
        return Err(LcError::conflict("turn is neither in-flight nor of unknown delivery", json!({"turn_id": t.id})));
    }
    let _ = insert_message(app, &t.conversation_id, Some(turn_id), "system", "turn abandoned by user", "system", false, None).await;
    emit_turn(app, turn_id).await;
    Ok(())
}


/// working -> idle 之後等 Stop hook 先到；沒等到才從終端收。
const FALLBACK_DELAY: Duration = Duration::from_secs(5);

/// Arm the 5s terminal-fallback timer after a working -> idle transition.
///
/// 計時器綁定**排定當下**在飛的那一回合（issue #216）：到點只收那一回合。上一回合已經被 Stop hook 收掉、這 5 秒內
/// 使用者又送出新的一則（CLI 還沒畫 spinner、herdr 還沒報 working），到點時「在飛的」是新回合，畫面上最後一段回覆卻是上一則的——
/// 不綁的話會把上一則的回覆收給剛送出的那一則，真正的回覆只能落到另一個外部回合。
pub async fn arm_fallback(app: &Arc<App>, run_id: &str, bot_id: &str) {
    arm_fallback_after(app, run_id, bot_id, FALLBACK_DELAY).await
}

async fn arm_fallback_after(app: &Arc<App>, run_id: &str, bot_id: &str, delay: Duration) {
    // 讀不到就不排：排了也不知道要收哪一回合，收錯比不收糟（這一回合有 poller 的閒置備援與 stuck watchdog 兜著）。
    let armed_for = match db::in_flight_turn(&app.db, run_id).await {
        Ok(turn) => turn.map(|t| t.id),
        Err(e) => {
            tracing::warn!(run_id, error = ?e, "cannot tell which turn the terminal fallback is for; not arming it");
            return;
        }
    };
    let mut timers = app.fallback_timers.lock().await;
    static NEXT_GENERATION: AtomicU64 = AtomicU64::new(0);
    let generation = NEXT_GENERATION.fetch_add(1, Ordering::Relaxed);
    timers.insert(run_id.to_string(), generation);
    let app2 = app.clone();
    let run_id = run_id.to_string();
    let bot_id = bot_id.to_string();
    tokio::spawn(async move {
        tokio::time::sleep(delay).await;
        let lock = app2.bot_lock(&bot_id).await;
        let _g = lock.lock().await;
        if app2.fallback_timers.lock().await.get(&run_id) != Some(&generation) {
            return;
        }
        match try_fallback(&app2, &run_id, armed_for.as_deref()).await {
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
///
/// `expected_turn`：呼叫端排定（或輪詢）當下看的那一回合，`None`＝排定當下沒有回合在飛。到點時在飛的不是它（排定之後才開的新回合）就不收
/// ——畫面上最後一段回覆屬於上一回合，留給新回合自己的 hook／下一次 edge／poller（issue #216）。
async fn try_fallback(app: &Arc<App>, run_id: &str, expected_turn: Option<&str>) -> anyhow::Result<bool> {
    let Some(run) = db::run(&app.db, run_id).await? else { return Ok(false) };
    let Some(turn) = db::in_flight_turn(&app.db, run_id).await? else { return Ok(false) };
    if expected_turn != Some(turn.id.as_str()) {
        tracing::debug!(run_id, turn = %turn.id, expected = ?expected_turn, "the turn in flight is not the one the fallback was armed for; leaving it");
        return Ok(false);
    }
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
    // grok（#222）走同一條：撞週限的畫面不是回覆，收成失敗、記撞限。
    let codex_hit = if matches!(bot.kind.as_str(), "codex" | "grok") {
        let grok = bot.kind == "grok";
        let limits = |t: &str| -> Vec<String> {
            if grok {
                grok_limit_notice_lines(t)
            } else {
                codex_usage_notice_lines(t).into_iter().filter(|n| codex_limit_hit_line(n).is_some()).collect()
            }
        };
        super::limit_banner::fallback_hit(run_id, &read.text, limits(&fresh), limits(&read.text))
    } else {
        None
    };

    // Derive the reply before SQLite's write lock (`pane_columns` RPC, `turn_echo_texts` other conn).
    let reply = if codex_hit.is_some() {
        None
    } else {
    // Only the `❯` row counts as echo; lines 2..n would be stored as the answer. 讀不到送了什麼就不收（#193），
    // 回合留在飛、下一次再來：當成什麼都沒送，我們自己的 prompt 就被當成 agent 的回覆存下來。
        let sent = turn_echo_texts(app, &turn.id).await?;
        #[cfg(test)]
        super::race_point::hit("fallback_after_echo", &turn.id).await;
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
    // 撞限直接 `in_flight -> failed`：先落在 `completed_fallback` 再改 failed 不是合法邊，trigger 會把整個交易擋掉（#109）。
    let res = if codex_hit.is_some() {
        super::turn_controller::fail_on(&mut tx, &turn.id, super::turn_controller::DeliveryOnFail::Keep, "§4.3 備援看到撞限橫幅").await?
    } else {
        super::turn_controller::set_status_on(&mut tx, &turn.id, "in_flight", "completed_fallback", "§4.3 終端快照備援").await?
    };
    if res != super::turn_controller::Outcome::Applied {
        return Ok(false);
    }
    tracing::info!(turn = %turn.id, "terminal fallback engaged");

    if let Some(hit) = codex_hit {
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
        // 記在這顆 bot 的主機與身分那一格。讀不到主機不退回 `local`；記不進去就欠著，派送前與 flush 照欠著的那一筆擋、
        // 之後每次查詢先補寫（#198）。回合已經收掉了，照樣回 `true`。
        if let Err(e) = crate::turn_error::mark_codex_limit_hit(app, &bot, &hit).await {
            tracing::warn!(turn = %turn.id, error = %e, "codex limit hit owed; it holds this bot until it is recorded");
        }
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

/// codex 的狀態列：`• {header} ({elapsed} • esc to interrupt)`，後面可能再接 ` · {訊息}`。header 預設 `Working`，
/// 壓縮時是 `Compacting context`，reasoning summary 開著時是 summary 的最新一行（0.155 起，#207）——只認 `Working (`
/// 的話，還在想的 codex 會被當成停了，備援把狀態列當回覆收掉回合。所以認結構：括號裡是經過時間、` • `、`… to interrupt`
/// （中斷鍵可以改綁）。格式照 codex `status_indicator_widget.rs`。
fn is_codex_working_line(s: &str) -> bool {
    let Some(rest) = s.trim().strip_prefix('•') else { return false };
    rest.match_indices(" (").any(|(i, _)| {
        let Some((elapsed, tail)) = rest[i + 2..].split_once(" • ") else { return false };
        is_codex_duration(elapsed) && tail.split_once(')').is_some_and(|(hint, _)| hint.ends_with(" to interrupt"))
    })
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
    if last_assistant_content(app, &conv).await?.as_deref().map(str::trim) == Some(reply.trim()) {
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

    /// #193：去重要看的上一則回覆讀不到，不當成「還沒有回覆」——以前照樣開一個外部回合，同一份回覆存第二次
    /// （這裡訊息寫不進去，留下一個沒有訊息的回合）。
    #[tokio::test]
    async fn an_unreadable_last_reply_is_not_taken_as_no_reply() {
        let c = child("idle", EXCHANGE, 0, 1).await;
        let app = c.env.app.clone();
        assert!(capture_hookless_turn_locked(&app, &c.run_id, false).await.unwrap());
        // herdr 對同一份回答報了第二次 `working -> idle`，游標又不在（重讀同一個畫面）：只有去重擋得住。
        sqlx::query("UPDATE runs SET last_read_tail_hash=NULL WHERE id=?").bind(&c.run_id).execute(&app.db).await.unwrap();

        sqlx::query("ALTER TABLE messages RENAME TO messages_unreadable").execute(&app.db).await.unwrap();
        assert!(capture_hookless_turn_locked(&app, &c.run_id, false).await.is_err(), "讀不到上一則回覆是錯，不是「沒有」");
        sqlx::query("ALTER TABLE messages_unreadable RENAME TO messages").execute(&app.db).await.unwrap();
        assert!(!capture_hookless_turn_locked(&app, &c.run_id, false).await.unwrap(), "讀得到了：同一份回覆");

        let turns: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM turns WHERE conversation_id=?").bind(&c.conv).fetch_one(&app.db).await.unwrap();
        assert_eq!(turns, 1, "沒有多開回合");
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
        mark_delivery(&app, &f.turn_id, Delivered::Submitted.record().unwrap(), &db::now()).await.unwrap();

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
        crate::lifecycle::prompt::mark_delivery(&app, &f.turn_id, Delivered::Handed.record().unwrap(), &db::now()).await.unwrap();
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
        crate::lifecycle::prompt::mark_delivery(&app, &f.turn_id, Delivered::Unverified.record().unwrap(), &db::now()).await.unwrap();
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

    /// #198 同類：讀不到主機就不打字——當成遠端會跳過本機 transcript 這種無損證據，改成盲打。一個字都還沒打，可重試。
    #[tokio::test]
    async fn a_prompt_whose_host_cannot_be_read_is_not_typed() {
        let f = fixture("claude", "").await;
        let _t = with_transcript(&f, wide()).await;
        let app = f.env.app.clone();
        db::set_pane_typed(&app.db, &f.run_id).await.unwrap();
        let (run, bot) = run_and_bot(&f).await;
        let client = client_for_run(&app, &run).await.unwrap();
        sqlx::query("ALTER TABLE projects RENAME TO projects_unreadable").execute(&app.db).await.unwrap();
        let out = deliver_prompt(&app, &client, &run, &bot, "Reply with PONG please", false, false).await.unwrap();
        sqlx::query("ALTER TABLE projects_unreadable RENAME TO projects").execute(&app.db).await.unwrap();
        assert_eq!(out, not("host_unreadable", true));
        assert_eq!(count(&f, "pane.send_text"), 0, "一個字都沒打");
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
        assert!(!db::claim_resend(&app.db, &f.turn_id, MAX_PROMPT_RESENDS).await.unwrap());
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

        let sent = turn_echo_texts(&app, &f.turn_id).await.unwrap();
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

        let sent = turn_echo_texts(&app, &f.turn_id).await.unwrap();
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
        assert_eq!(out, Some((Resend::Sent, None)), "第二次用退回的額度送出去");
        assert_eq!(count(&f, "pane.send_text"), 1, "只送了一次");
        assert!(!db::claim_resend(&app.db, &f.turn_id, MAX_PROMPT_RESENDS).await.unwrap(), "額度這次真的用掉了");
    }

    /// review3 c3 M5：重送打了字、Enter 被吞，字還在框裡（`Unproven("still_in_box")`）——不是「agent 沒反應」。
    /// turn 記成 unknown＋說明，接下來的 `fail_stalled_turn` 不能把它判 failed。
    #[tokio::test]
    async fn an_unproven_resend_leaves_the_turn_unknown_not_failed() {
        let f = fixture("claude", "").await;
        live(&f, crate::testing::LivePane { transcript: vec!["⏺ 先前的回覆".into()], swallow_enter: true, ..wide() });
        let app = f.env.app.clone();
        db::set_pane_typed(&app.db, &f.run_id).await.unwrap();
        app.stall_timers.lock().await.insert(f.run_id.clone(), 11);
        let sent = vec!["Reply with PONG please".to_string()];

        let out = resend_with_retry(&app, &f.run_id, &f.bot_id, &f.turn_id, &sent, 11, Duration::from_millis(1)).await;
        assert_eq!(out, Some((Resend::Unproven, None)));
        assert_eq!(count(&f, "pane.send_text"), 1, "打過一次字");
        let t = turn(&app, &f.turn_id).await;
        assert_eq!((t.status.as_str(), t.delivery.as_str()), ("in_flight", "unknown"));
        let msg: String = sqlx::query_scalar("SELECT content FROM messages WHERE turn_id=? AND role='system'")
            .bind(&f.turn_id)
            .fetch_one(&app.db)
            .await
            .unwrap();
        assert!(msg.contains("證明不了有送出") && msg.contains("still_in_box") && msg.contains("輸入框"), "{msg}");

        fail_stalled_turn(&app, &f.run_id, &f.bot_id, &f.turn_id, false, false, None).await.unwrap();
        assert_eq!(turn(&app, &f.turn_id).await.status, "in_flight", "送達不明的不判失敗");
    }

    /// #193：重送打過字、證明不了，`delivery='unknown'` 卻寫不進去——以前只記一行 warning，接著照 `delivery='ok'`
    /// 判 failed，打過的字可能已經被收下，AGM 重派同一件事。現在欠著、這一輪不判；寫得進去的下一輪補上，不判失敗、不重打。
    #[tokio::test]
    async fn an_unproven_resend_that_cannot_be_recorded_is_owed_not_failed() {
        let f = fixture("claude", "").await;
        live(&f, crate::testing::LivePane { transcript: vec!["⏺ 先前的回覆".into()], swallow_enter: true, ..wide() });
        let app = f.env.app.clone();
        db::set_pane_typed(&app.db, &f.run_id).await.unwrap();
        app.stall_timers.lock().await.insert(f.run_id.clone(), 12);
        sqlx::query(
            "CREATE TRIGGER refuse_unknown BEFORE UPDATE OF delivery ON turns WHEN NEW.delivery='unknown'
             BEGIN SELECT RAISE(ABORT, 'injected: cannot record unknown delivery'); END",
        )
        .execute(&app.db)
        .await
        .unwrap();
        let mut st = Stall::default();
        assert_eq!(stall_round(&app, &f.run_id, &f.bot_id, &f.turn_id, 12, &mut st, Duration::from_millis(1)).await, StallRound::Unsure);
        assert_eq!(count(&f, "pane.send_text"), 1, "打過一次字");
        let t = turn(&app, &f.turn_id).await;
        assert_eq!((t.status.as_str(), t.delivery.as_str()), ("in_flight", "ok"), "記不成 unknown：欠著，不判失敗");

        sqlx::query("DROP TRIGGER refuse_unknown").execute(&app.db).await.unwrap();
        assert_eq!(stall_round(&app, &f.run_id, &f.bot_id, &f.turn_id, 12, &mut st, Duration::from_millis(1)).await, StallRound::Done);
        let t = turn(&app, &f.turn_id).await;
        assert_eq!((t.status.as_str(), t.delivery.as_str()), ("in_flight", "unknown"), "補上了：送達不明，不判失敗");
        assert_eq!(count(&f, "pane.send_text"), 1, "不重打第二次");
    }

    /// #193：期限那一輪讀不到送了什麼——以前當成什麼都沒送：框裡的字不補 Enter、沒到的不重送，接著判失敗。
    /// 現在這一輪不判（`Unsure`）；讀得到的下一輪照常，字還在框裡就補 Enter。
    #[tokio::test]
    async fn a_stall_round_that_cannot_read_what_was_sent_does_not_fail_the_turn() {
        let rule = "─".repeat(82);
        let screen = format!("⏺ 先前的回覆\n\n✻ Worked for 2s · done 3:34 PM\n{rule}\n❯ Reply with PONG\n{rule}\n  ⏵⏵ bypass permissions on (shift+tab to cycle)\n");
        let f = fixture("claude", &screen).await;
        let app = f.env.app.clone();
        app.stall_timers.lock().await.insert(f.run_id.clone(), 31);
        sqlx::query("ALTER TABLE messages RENAME TO messages_unreadable").execute(&app.db).await.unwrap();
        // 讀完之後 DB 就恢復：接下來判失敗是寫得進去的。
        let a = app.clone();
        crate::lifecycle::race_point::arm("stall_after_nudge", &f.turn_id, move || async move {
            sqlx::query("ALTER TABLE messages_unreadable RENAME TO messages").execute(&a.db).await.unwrap();
        });
        let mut st = Stall::default();
        assert_eq!(stall_round(&app, &f.run_id, &f.bot_id, &f.turn_id, 31, &mut st, Duration::from_millis(1)).await, StallRound::Unsure);
        assert_eq!(turn(&app, &f.turn_id).await.status, "in_flight", "判斷不了就不判失敗");
        assert_eq!(count(&f, "pane.send_keys"), 0);

        assert_eq!(stall_round(&app, &f.run_id, &f.bot_id, &f.turn_id, 31, &mut st, Duration::from_millis(1)).await, StallRound::Done);
        assert_eq!(count(&f, "pane.send_keys"), 1, "讀得到了：字還在框裡，補 Enter");
        assert_eq!(turn(&app, &f.turn_id).await.status, "failed", "補了 Enter 還是沒反應：照常判失敗");
        let msg: String = sqlx::query_scalar("SELECT content FROM messages WHERE turn_id=? AND role='system'")
            .bind(&f.turn_id)
            .fetch_one(&app.db)
            .await
            .unwrap();
        assert!(msg.contains("已嘗試補送一次 Enter"), "{msg}");
    }

    /// #193：重送前讀不到 run／bot、重送額度寫不進去，都是 `Unreadable`，不是 `Skipped`（`Skipped` 之後就判失敗了）。
    #[tokio::test]
    async fn a_resend_that_cannot_read_or_claim_is_unreadable_not_skipped() {
        let f = fixture("claude", "").await;
        live(&f, crate::testing::LivePane { transcript: vec!["⏺ 先前的回覆".into()], ..wide() });
        let app = f.env.app.clone();
        db::set_pane_typed(&app.db, &f.run_id).await.unwrap();
        let sent = vec!["Reply with PONG".to_string()];

        sqlx::query("ALTER TABLE bots RENAME TO bots_unreadable").execute(&app.db).await.unwrap();
        assert_eq!(resend_lost_prompt(&app, &f.run_id, &f.turn_id, &sent).await, Resend::Unreadable, "讀不到 bot");
        sqlx::query("ALTER TABLE bots_unreadable RENAME TO bots").execute(&app.db).await.unwrap();

        sqlx::query("CREATE TRIGGER refuse_claim BEFORE UPDATE OF resend_count ON turns BEGIN SELECT RAISE(ABORT, 'injected: cannot claim a resend'); END")
            .execute(&app.db)
            .await
            .unwrap();
        assert_eq!(resend_lost_prompt(&app, &f.run_id, &f.turn_id, &sent).await, Resend::Unreadable, "額度寫不進去不是「用完了」");
        assert_eq!(count(&f, "pane.send_text"), 0);
        sqlx::query("DROP TRIGGER refuse_claim").execute(&app.db).await.unwrap();
        assert!(resend_lost_prompt(&app, &f.run_id, &f.turn_id, &sent).await.sent(), "寫得進去了：照常重送");
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
        let Some((Resend::Blocked { .. }, Some(why))) = out else { panic!("expected blocked twice, got {out:?}") };
        assert_eq!(count(&f, "pane.send_text"), 0, "一個字都沒打");
        assert!(db::claim_resend(&app.db, &f.turn_id, MAX_PROMPT_RESENDS).await.unwrap(), "額度還在（兩次都退回了）");

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

    /// codex 撞限時終端備援要把回合收成 failed 並說明原因。以前先改成 `completed_fallback` 再改 `failed`，
    /// 而 turn 的轉移 trigger 沒有 `completed_fallback -> failed` 這條邊：整個交易被擋掉，回合留在
    /// in_flight 直到 stuck watchdog，撞限的說明也沒寫進對話。
    #[tokio::test]
    async fn a_codex_limit_banner_fails_the_turn_through_the_fallback() {
        let banner = "■ You've hit your usage limit. Upgrade to Pro, or try again at Sep 19th, 2026 6:43 PM.";
        let f = fixture("codex", &format!("› Reply with PONG\n\n{banner}\n\n› \n")).await;
        let app = f.env.app.clone();

        assert!(try_fallback(&app, &f.run_id, Some(&f.turn_id)).await.expect("備援不能因為轉移被擋而失敗"), "回合要被收掉");

        assert_eq!(turn(&app, &f.turn_id).await.status, "failed");
        let notes: Vec<String> = sqlx::query_scalar("SELECT content FROM messages WHERE turn_id=? AND role='system'")
            .bind(&f.turn_id)
            .fetch_all(&app.db)
            .await
            .unwrap();
        assert!(notes.iter().any(|n| n.contains("hit your usage limit")), "{notes:?}");
    }
}

#[cfg(test)]
mod progress_poll_tests {
    //! #193：終端側的安全網讀 DB 一時失敗，不能當成「回合沒了」就收手。故障用改表名／trigger 注入；poller 的輪詢只對
    //! 這幾個 run 調快（[`fast`]），每一輪讀 DB 之前的 `progress_poll` 注入點讓故障確定落在某一輪、下一輪恢復。
    use super::*;
    use crate::lifecycle::race_point;
    use crate::testing as tt;
    use std::collections::HashMap;
    use std::sync::{Mutex, OnceLock};

    fn intervals() -> &'static Mutex<HashMap<String, Duration>> {
        static M: OnceLock<Mutex<HashMap<String, Duration>>> = OnceLock::new();
        M.get_or_init(Default::default)
    }

    pub(super) fn interval(run_id: &str) -> Option<Duration> {
        intervals().lock().ok()?.get(run_id).copied()
    }

    fn fast(run_id: &str) {
        intervals().lock().unwrap().insert(run_id.to_string(), Duration::from_millis(10));
    }

    const PONG: &str = "❯ Reply with PONG\n⏺ PONG\n✻ Worked for 5s · done 1:07 AM\n──────\n❯\n";
    const BUSY: &str = "❯ Reply with PONG\n✢ Baking… (3s · esc to interrupt)\n──────\n❯\n";

    struct F {
        env: tt::Env,
        bot_id: String,
        run_id: String,
        turn_id: String,
    }

    /// 一顆在跑的 claude（`pane-p`），一筆在飛、送達 ok 的回合，對話裡是 `prompt`。
    async fn in_flight(screen: &str, prompt: &str) -> F {
        let env = tt::env().await;
        let app = env.app.clone();
        let bot = tt::claude_bot(&app, &env.project_id, "polled").await;
        let conv = db::conversation_id(&app.db, &bot.id).await.unwrap();
        let run_id = db::ulid();
        sqlx::query(
            "INSERT INTO runs (id, bot_id, state, agent_status, pane_id, herdr_session, started_at)
             VALUES (?,?,'running','idle','pane-p','test',?)",
        )
        .bind(&run_id)
        .bind(&bot.id)
        .bind(db::now())
        .execute(&app.db)
        .await
        .unwrap();
        let turn_id = db::ulid();
        sqlx::query(
            "INSERT INTO turns (id, conversation_id, run_id, origin, status, delivery, created_at) VALUES (?,?,?,'web','in_flight','ok',?)",
        )
        .bind(&turn_id)
        .bind(&conv)
        .bind(&run_id)
        .bind(db::now())
        .execute(&app.db)
        .await
        .unwrap();
        sqlx::query("INSERT INTO messages (id, conversation_id, turn_id, role, content, source, created_at) VALUES (?,?,?,'user',?,'web',?)")
            .bind(db::ulid())
            .bind(&conv)
            .bind(&turn_id)
            .bind(prompt)
            .bind(db::now())
            .execute(&app.db)
            .await
            .unwrap();
        env.herdr.set_screen("pane-p", screen);
        F { env, bot_id: bot.id, run_id, turn_id }
    }

    async fn unreadable(app: &Arc<App>, table: &str, yes: bool) {
        let sql = if yes { format!("ALTER TABLE {table} RENAME TO {table}_unreadable") } else { format!("ALTER TABLE {table}_unreadable RENAME TO {table}") };
        sqlx::query(&sql).execute(&app.db).await.unwrap();
    }

    /// 還沒恢復才恢復（注入點可能走到、也可能沒走到）。
    async fn readable_again(app: &Arc<App>, table: &str) {
        let _ = sqlx::query(&format!("ALTER TABLE {table}_unreadable RENAME TO {table}")).execute(&app.db).await;
    }

    /// 這一輪（`progress_poll`）先跑 `now`，下一輪跑 `next` 並通知測試。poller 在這一輪之後就退出的話，永遠等不到通知。
    fn this_poll_then_next<A, AF, B, BF>(run_id: &str, now: A, next: B) -> tokio::sync::oneshot::Receiver<()>
    where
        A: FnOnce() -> AF + Send + 'static,
        AF: std::future::Future<Output = ()> + Send + 'static,
        B: FnOnce() -> BF + Send + 'static,
        BF: std::future::Future<Output = ()> + Send + 'static,
    {
        let (tx, rx) = tokio::sync::oneshot::channel();
        let run = run_id.to_string();
        race_point::arm("progress_poll", run_id, move || async move {
            now().await;
            race_point::arm("progress_poll", &run, move || async move {
                next().await;
                let _ = tx.send(());
            });
        });
        rx
    }

    async fn still_polled_after(rx: tokio::sync::oneshot::Receiver<()>) {
        tokio::time::timeout(Duration::from_secs(10), rx)
            .await
            .expect("讀錯之後 poller 應該還在：下一輪沒有來，它在讀錯那一輪就退出了")
            .unwrap();
    }

    async fn status(app: &Arc<App>, turn_id: &str) -> String {
        sqlx::query_scalar("SELECT status FROM turns WHERE id=?").bind(turn_id).fetch_one(&app.db).await.unwrap()
    }

    async fn pollers(app: &Arc<App>, run_id: &str) -> Option<bool> {
        app.progress_pollers.lock().await.get(run_id).map(|h| h.is_finished())
    }

    async fn wait_for_status(app: &Arc<App>, turn_id: &str, want: &str) {
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while status(app, turn_id).await != want {
            assert!(std::time::Instant::now() < deadline, "回合沒有變成 {want}（pane 閒著、沒有 Stop hook，閒置備援應該收掉它）");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    async fn wait_until_poller_gone(app: &Arc<App>, run_id: &str) {
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while pollers(app, run_id).await.is_some() {
            assert!(std::time::Instant::now() < deadline, "回合收掉了，poller 應該退出");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    /// 驗收 1、2：某一輪讀 `in_flight_turn` 失敗（SQLite busy／I/O error），一輪之後 DB 恢復——poller 還活著；之後 pane
    /// 閒著、沒有 Stop hook，閒置備援照樣把回合收掉。以前讀錯＝`still=false`＝退出，那一回合從此沒人收。
    #[tokio::test]
    async fn a_transient_in_flight_read_error_does_not_end_the_poller() {
        let f = in_flight(PONG, "Reply with PONG").await;
        let app = f.env.app.clone();
        fast(&f.run_id);
        let (a, b) = (app.clone(), app.clone());
        let rx = this_poll_then_next(&f.run_id, move || async move { unreadable(&a, "turns", true).await }, move || async move {
            unreadable(&b, "turns", false).await
        });
        arm_progress(&app, &f.run_id, &f.bot_id, &f.turn_id).await;
        still_polled_after(rx).await;

        wait_for_status(&app, &f.turn_id, "completed_fallback").await;
        let reply: String = sqlx::query_scalar("SELECT content FROM messages WHERE turn_id=? AND role='assistant'")
            .bind(&f.turn_id)
            .fetch_one(&app.db)
            .await
            .unwrap();
        assert_eq!(reply, "PONG");
        wait_until_poller_gone(&app, &f.run_id).await;
    }

    /// 驗收 3：讀到了、確定不是這一回合（`Ok(None)`）才停。
    #[tokio::test]
    async fn the_poller_stops_once_the_turn_is_really_closed() {
        let f = in_flight(BUSY, "Reply with PONG").await;
        let app = f.env.app.clone();
        fast(&f.run_id);
        let rx = this_poll_then_next(&f.run_id, || async {}, || async {});
        arm_progress(&app, &f.run_id, &f.bot_id, &f.turn_id).await;
        still_polled_after(rx).await;
        assert_eq!(pollers(&app, &f.run_id).await, Some(false), "回合還在飛：poller 在跑");

        sqlx::query("UPDATE turns SET status='completed', completed_at=? WHERE id=?").bind(db::now()).bind(&f.turn_id).execute(&app.db).await.unwrap();
        wait_until_poller_gone(&app, &f.run_id).await;
    }

    /// 驗收 4：讀 `runs` 一時失敗一樣重讀，不退出；恢復之後還是同一個 poller。
    #[tokio::test]
    async fn a_transient_run_read_error_does_not_end_the_poller() {
        let f = in_flight(BUSY, "Reply with PONG").await;
        let app = f.env.app.clone();
        fast(&f.run_id);
        let (a, b) = (app.clone(), app.clone());
        let rx = this_poll_then_next(&f.run_id, move || async move { unreadable(&a, "runs", true).await }, move || async move {
            unreadable(&b, "runs", false).await
        });
        arm_progress(&app, &f.run_id, &f.bot_id, &f.turn_id).await;
        still_polled_after(rx).await;
        assert_eq!(pollers(&app, &f.run_id).await, Some(false));
        assert_eq!(app.progress_pollers.lock().await.len(), 1, "沒有多出第二個 poller");
    }

    /// 驗收 4：掛上的那一刻讀不到 bot，以前 poller 直接不開始；現在下一輪再讀。
    #[tokio::test]
    async fn a_bot_that_cannot_be_read_when_the_poller_starts_is_read_again() {
        let f = in_flight(BUSY, "Reply with PONG").await;
        let app = f.env.app.clone();
        fast(&f.run_id);
        unreadable(&app, "bots", true).await;
        let b = app.clone();
        // 第一輪 bot 還讀不到，第二輪之前恢復。
        let rx = this_poll_then_next(&f.run_id, || async {}, move || async move { unreadable(&b, "bots", false).await });
        arm_progress(&app, &f.run_id, &f.bot_id, &f.turn_id).await;
        still_polled_after(rx).await;
        let again = this_poll_then_next(&f.run_id, || async {}, || async {});
        still_polled_after(again).await;
        assert_eq!(pollers(&app, &f.run_id).await, Some(false));
        assert_eq!(app.progress_pollers.lock().await.len(), 1);
    }

    /// 開始新回合時上一回合的錯誤標記清不掉（寫入失敗）：poller 之後再清。留著的話，這一回合斷在同一句錯誤上會被
    /// `turn_error::capture` 的「同一則只記一次」吞掉。
    #[tokio::test]
    async fn the_previous_turns_error_is_cleared_once_the_write_goes_through() {
        let f = in_flight(BUSY, "Reply with PONG").await;
        let app = f.env.app.clone();
        fast(&f.run_id);
        sqlx::query("UPDATE runs SET turn_error='API Error: Connection lost mid-response.' WHERE id=?").bind(&f.run_id).execute(&app.db).await.unwrap();
        sqlx::query(
            "CREATE TRIGGER refuse_clear BEFORE UPDATE OF turn_error ON runs WHEN NEW.turn_error IS NULL
             BEGIN SELECT RAISE(ABORT, 'injected: cannot clear runs.turn_error'); END",
        )
        .execute(&app.db)
        .await
        .unwrap();
        let a = app.clone();
        let rx = this_poll_then_next(&f.run_id, move || async move { sqlx::query("DROP TRIGGER refuse_clear").execute(&a.db).await.unwrap(); }, || async {});
        arm_progress(&app, &f.run_id, &f.bot_id, &f.turn_id).await;
        still_polled_after(rx).await;
        let left: Option<String> = sqlx::query_scalar("SELECT turn_error FROM runs WHERE id=?").bind(&f.run_id).fetch_one(&app.db).await.unwrap();
        assert_eq!(left, None, "寫得進去之後就清掉了");
    }

    /// 備援收回合之前讀不到送了什麼：不收（回合留在飛），不把我們自己的 prompt 當成 agent 的回覆存下來。
    #[tokio::test]
    async fn a_fallback_that_cannot_read_what_was_sent_closes_nothing() {
        let f = in_flight(PONG, "Reply with PONG").await;
        let app = f.env.app.clone();
        unreadable(&app, "messages", true).await;
        // 讀完「送了什麼」之後 DB 就恢復：接著寫得進去的話，錯的回覆就真的存下來了。
        let a = app.clone();
        race_point::arm("fallback_after_echo", &f.turn_id, move || async move { readable_again(&a, "messages").await });

        let first = {
            let lock = app.bot_lock(&f.bot_id).await;
            let _g = lock.lock().await;
            try_fallback(&app, &f.run_id, Some(&f.turn_id)).await
        };
        assert!(first.is_err(), "讀不到送了什麼是錯，不是「什麼都沒送」：{first:?}");
        assert_eq!(status(&app, &f.turn_id).await, "in_flight");
        readable_again(&app, "messages").await;

        assert!(try_fallback(&app, &f.run_id, Some(&f.turn_id)).await.unwrap(), "讀得到了：照常收");
        let reply: String = sqlx::query_scalar("SELECT content FROM messages WHERE turn_id=? AND role='assistant'")
            .bind(&f.turn_id)
            .fetch_one(&app.db)
            .await
            .unwrap();
        assert_eq!(reply, "PONG");
    }
}

#[cfg(test)]
mod codex_limit_fallback_tests {
    //! #198：備援看到 codex 撞限橫幅，記撞限走 `turn_error::mark_codex_limit_hit`（讀不到主機、算不準 key 都欠著），不退回 `local`、不猜 key。
    use super::*;
    use crate::testing as tt;

    #[tokio::test]
    async fn a_limit_banner_whose_key_is_unknown_is_owed_by_the_fallback() {
        let env = tt::env().await;
        let app = env.app.clone();
        let bot = tt::claude_bot(&app, &env.project_id, "cx").await;
        sqlx::query("UPDATE bots SET kind='codex', identity='cx0' WHERE id=?").bind(&bot.id).execute(&app.db).await.unwrap();
        let bot = db::bot(&app.db, &bot.id).await.unwrap().unwrap();
        let run = tt::fake_run(&app, &bot.id).await;
        let turn = crate::lifecycle::run_state::a_turn(&app, &bot.id, Some(&run), "in_flight").await;
        let banner = "■ You've hit your usage limit. Upgrade to Pro, or try again at Sep 19th, 2099 6:43 PM.";
        env.herdr.set_screen(&format!("pane-{}", bot.id), &format!("› 派工\n\n{banner}\n\n› \n"));

        assert!(try_fallback(&app, &run, Some(&turn)).await.unwrap(), "回合照樣收");
        assert_eq!(crate::lifecycle::run_state::turn_status(&app, &turn).await, "failed");
        assert!(app.quotas.lock().await.values().all(|q| q.limit_hit.is_none()), "身分表還沒偵測完：沒有猜一格 `codex:cx0` 寫下去");
        assert!(crate::quota::try_limit_hit_for_bot(&app, &bot).await.unwrap().is_some(), "欠著照擋");
    }
}

#[cfg(test)]
mod codex_0155_fallback_tests {
    //! #207：codex 0.155.1 真畫面（見 `screen.rs` 的 `codex_0155_screen_tests`）。
    use super::*;
    use crate::testing as tt;

    const WORKING: &str = include_str!("fixtures/codex-0.155-working.txt");
    const WORKING_SUMMARY: &str = include_str!("fixtures/codex-0.155-working-summary.txt");
    const FINISHED: &str = include_str!("fixtures/codex-0.155-finished.txt");

    async fn codex_turn(screen: &str, prompt: &str) -> (tt::Env, String, String) {
        let env = tt::env().await;
        let app = env.app.clone();
        let bot = tt::claude_bot(&app, &env.project_id, "cx").await;
        sqlx::query("UPDATE bots SET kind='codex' WHERE id=?").bind(&bot.id).execute(&app.db).await.unwrap();
        let run = tt::fake_run(&app, &bot.id).await;
        let turn = crate::lifecycle::run_state::a_turn(&app, &bot.id, Some(&run), "in_flight").await;
        sqlx::query("UPDATE turns SET prompt_text=? WHERE id=?").bind(prompt).bind(&turn).execute(&app.db).await.unwrap();
        env.herdr.set_screen(&format!("pane-{}", bot.id), screen);
        (env, run, turn)
    }

    /// 真畫面回合中：還在想，備援不收。summary 開著時字頭在撞限前仍是 `Working`；疊上 summary 字頭一樣不收。
    #[tokio::test]
    async fn a_codex_still_thinking_under_a_summary_header_is_not_closed() {
        for screen in [WORKING, WORKING_SUMMARY] {
            let (env, run, turn) = codex_turn(screen, "Reply with only the word PONG. Do not use tools.").await;
            let app = env.app.clone();
            assert!(!try_fallback(&app, &run, Some(&turn)).await.unwrap(), "還在想：不收");
            assert_eq!(crate::lifecycle::run_state::turn_status(&app, &turn).await, "in_flight");
        }
        let summary = WORKING.replace("Working (0s • esc to interrupt)", "Planning the fix for the poller (12s • esc to interrupt)");
        let (env, run, turn) = codex_turn(&summary, "Reply with only the word PONG. Do not use tools.").await;
        let app = env.app.clone();
        assert!(!try_fallback(&app, &run, Some(&turn)).await.unwrap(), "summary 字頭：不收");
        assert_eq!(crate::lifecycle::run_state::turn_status(&app, &turn).await, "in_flight");
    }

    /// 真畫面回合結束：存下來的回覆不帶完成時間那一行。
    #[tokio::test]
    async fn a_finished_codex_turn_is_stored_without_its_completion_line() {
        let (env, run, turn) = codex_turn(FINISHED, "Reply with exactly: CODEX OK").await;
        let app = env.app.clone();
        assert!(try_fallback(&app, &run, Some(&turn)).await.unwrap());
        let reply: String = sqlx::query_scalar("SELECT content FROM messages WHERE turn_id=? AND role='assistant'").bind(&turn).fetch_one(&app.db).await.unwrap();
        assert_eq!(reply, "CODEX OK");
        assert!(!reply.contains("done"));
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

#[cfg(test)]
mod fallback_binding_tests {
    //! issue #216：§4.3 備援計時器綁定排定當下的回合。全部依序真的呼叫（`arm_fallback_after` 排計時、真的開新回合、
    //! 真的等計時到點走完 `try_fallback`），不靠 trigger 或注入點插窄窗——現況（計時到點抓「當下在飛的」）第一條就紅。
    use super::*;
    use crate::lifecycle::run_state::{a_turn, turn_status};
    use crate::testing as tt;

    /// 計時縮短到 100ms；別的測試的 5 秒不動。
    const SHORT: Duration = Duration::from_millis(100);
    /// 上一則（A）的回覆還在畫面上、輸入框是空的：沒有 spinner、不是工具進度——備援收得下去。
    const A_REPLY: &str = "❯ 派工\n⏺ 上一則的回覆 ALPHA\n✻ Worked for 5s · done 1:07 AM\n──────\n❯\n";

    struct F {
        env: tt::Env,
        bot_id: String,
        run_id: String,
    }

    /// 一顆在跑的 claude，pane 上是 A 的回覆。
    async fn fixture() -> F {
        let env = tt::env().await;
        let bot = tt::claude_bot(&env.app, &env.project_id, "armed").await;
        let run_id = tt::fake_run(&env.app, &bot.id).await;
        env.herdr.set_screen(&format!("pane-{}", bot.id), A_REPLY);
        F { env, bot_id: bot.id, run_id }
    }

    /// Stop hook 收掉一回合（`hookrecv` 做的事：in_flight -> completed）。
    async fn stop_hook_closes(f: &F, turn: &str) {
        let done = super::super::turn_controller::set_status(&f.env.app.db, turn, "in_flight", "completed", "test：Stop hook").await.unwrap();
        assert_eq!(done, super::super::turn_controller::Outcome::Applied);
    }

    async fn assistant_messages(f: &F) -> Vec<(Option<String>, String)> {
        let conv = db::conversation_id(&f.env.app.db, &f.bot_id).await.unwrap();
        sqlx::query_as("SELECT turn_id, content FROM messages WHERE conversation_id=? AND role='assistant' ORDER BY created_at")
            .bind(conv)
            .fetch_all(&f.env.app.db)
            .await
            .unwrap()
    }

    /// 排了計時、而且等它真的走完（計時任務最後會把自己這一代從表裡拿掉）。走不完＝這條測試沒測到東西，不算過。
    async fn arm_and_wait(f: &F, delay: Duration) {
        let app = &f.env.app;
        arm_fallback_after(app, &f.run_id, &f.bot_id, delay).await;
        wait_for_timer(f).await;
    }

    async fn armed(f: &F) -> bool {
        f.env.app.fallback_timers.lock().await.contains_key(&f.run_id)
    }

    async fn wait_for_timer(f: &F) {
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while armed(f).await {
            assert!(std::time::Instant::now() < deadline, "備援計時器沒有走完");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    /// 事故（2026-09-19 真機）：A 的 Stop hook 收掉 A → 畫面 idle，排備援 → 計時內使用者送出 B，CLI 還沒畫 spinner →
    /// 到點時「在飛的」是 B，畫面上最後一段回覆是 A 的。B 不能被收成 `completed_fallback`、也不能存 A 的回覆。
    #[tokio::test]
    async fn a_turn_sent_after_the_idle_edge_is_not_closed_with_the_previous_reply() {
        let f = fixture().await;
        let app = f.env.app.clone();
        let a = a_turn(&app, &f.bot_id, Some(&f.run_id), "in_flight").await;
        stop_hook_closes(&f, &a).await;

        arm_fallback_after(&app, &f.run_id, &f.bot_id, SHORT).await;
        assert!(armed(&f).await, "排定了計時");
        let b = a_turn(&app, &f.bot_id, Some(&f.run_id), "in_flight").await;
        wait_for_timer(&f).await;

        assert_eq!(turn_status(&app, &b).await, "in_flight", "B 留給自己的 hook／下一次 edge／poller");
        assert_eq!(assistant_messages(&f).await, vec![], "沒有把 A 的回覆存成任何人的回覆");
        assert_eq!(turn_status(&app, &a).await, "completed");
    }

    /// 同一件事、Stop hook 晚一步：edge 進來時 A 還在飛（排定綁 A），A 被 hook 收掉、B 送出，到點在飛的是 B。
    #[tokio::test]
    async fn the_timer_stays_bound_to_the_turn_that_was_in_flight_when_it_was_armed() {
        let f = fixture().await;
        let app = f.env.app.clone();
        let a = a_turn(&app, &f.bot_id, Some(&f.run_id), "in_flight").await;

        arm_fallback_after(&app, &f.run_id, &f.bot_id, SHORT).await;
        stop_hook_closes(&f, &a).await;
        let b = a_turn(&app, &f.bot_id, Some(&f.run_id), "in_flight").await;
        wait_for_timer(&f).await;

        assert_eq!(turn_status(&app, &b).await, "in_flight");
        assert_eq!(assistant_messages(&f).await, vec![]);
    }

    /// 對照組：hook 一直沒來、到點在飛的就是排定當下那一回合——備援照舊把它收掉（功能沒被綁壞）。
    #[tokio::test]
    async fn the_turn_the_timer_was_armed_for_is_still_closed_from_the_pane() {
        let f = fixture().await;
        let app = f.env.app.clone();
        let a = a_turn(&app, &f.bot_id, Some(&f.run_id), "in_flight").await;

        arm_and_wait(&f, SHORT).await;

        assert_eq!(turn_status(&app, &a).await, "completed_fallback");
        assert_eq!(assistant_messages(&f).await, vec![(Some(a), "上一則的回覆 ALPHA".to_string())]);
    }

    /// 新回合自己的 working -> idle 會排一個新的計時（換掉舊的一代）：那一個綁 B，B 的回覆照樣收得到。
    #[tokio::test]
    async fn a_later_idle_edge_arms_the_timer_for_the_new_turn() {
        let f = fixture().await;
        let app = f.env.app.clone();
        let a = a_turn(&app, &f.bot_id, Some(&f.run_id), "in_flight").await;
        stop_hook_closes(&f, &a).await;
        arm_fallback_after(&app, &f.run_id, &f.bot_id, Duration::from_millis(400)).await;

        let b = a_turn(&app, &f.bot_id, Some(&f.run_id), "in_flight").await;
        f.env.herdr.set_screen(&format!("pane-{}", f.bot_id), "❯ 派工\n⏺ B 的回覆 BRAVO\n✻ Worked for 5s · done 1:08 AM\n──────\n❯\n");
        arm_fallback_after(&app, &f.run_id, &f.bot_id, SHORT).await;
        wait_for_timer(&f).await;

        assert_eq!(turn_status(&app, &b).await, "completed_fallback");
        assert_eq!(assistant_messages(&f).await, vec![(Some(b), "B 的回覆 BRAVO".to_string())]);
        // 舊的那一代（400ms）到點時已經被換掉，什麼都不動。
        tokio::time::sleep(Duration::from_millis(500)).await;
        assert_eq!(assistant_messages(&f).await.len(), 1);
    }

    /// 讀不到「現在哪一回合在飛」就不排：排了也不知道要收哪一回合，收錯比不收糟。回合留在飛，由 poller 的閒置備援與 stuck watchdog 兜著。
    #[tokio::test]
    async fn no_timer_is_armed_when_it_cannot_tell_which_turn_is_in_flight() {
        let f = fixture().await;
        let app = f.env.app.clone();
        let a = a_turn(&app, &f.bot_id, Some(&f.run_id), "in_flight").await;

        tt::make_table_unreadable(&app, "turns").await;
        arm_fallback_after(&app, &f.run_id, &f.bot_id, SHORT).await;
        tt::make_table_readable(&app, "turns").await;

        assert!(!armed(&f).await, "讀不到就沒有排計時");
        assert_eq!(turn_status(&app, &a).await, "in_flight");
    }

    /// poller 的閒置備援（`arm_progress`）同一條規則：它問的是自己那一回合，在飛的若是別的就不收。
    #[tokio::test]
    async fn a_fallback_only_closes_the_turn_it_was_asked_about() {
        let f = fixture().await;
        let app = f.env.app.clone();
        let a = a_turn(&app, &f.bot_id, Some(&f.run_id), "in_flight").await;
        stop_hook_closes(&f, &a).await;
        let b = a_turn(&app, &f.bot_id, Some(&f.run_id), "in_flight").await;

        assert!(!try_fallback(&app, &f.run_id, Some(&a)).await.unwrap(), "問的是 A，在飛的是 B：不收");
        assert!(!try_fallback(&app, &f.run_id, None).await.unwrap(), "排定當下沒有回合在飛，在飛的是後來才開的 B：不收");
        assert_eq!(turn_status(&app, &b).await, "in_flight");
        assert_eq!(assistant_messages(&f).await, vec![]);

        assert!(try_fallback(&app, &f.run_id, Some(&b)).await.unwrap(), "問的就是 B：照舊收");
        assert_eq!(turn_status(&app, &b).await, "completed_fallback");
    }
}

#[cfg(test)]
mod grok_limit_screen_tests {
    //! #222：grok 撞週限。6d5e74ab 讓掃描（`capture_codex_usage_notices`）認得那兩句，這裡驗**它有沒有被叫到**：
    //! herdr 把這張畫面判成 `blocked`（不是 working→idle），備援與掃描都靠 idle 邊觸發，blocked 的 bot 沒人看。
    use super::*;
    use crate::testing as tt;
    use std::time::Duration;

    /// 票上貼的網頁「終端擷取」原文（2026-09-19 16:26，grok 1.0.34）。
    const TICKET_SCREEN: &str = "\
┃  You hit your weekly limit.
┃
┃  1 (○) Upgrade tier      Upgrade to a higher tier for more usage
┃  2 (○) Buy more credits  Purchase credits to keep using Grok Build
┃  3 (○) Try Again         Resubmit the last prompt once you have usage again
┃
┃  ↑/↓ navigate · y copy
Enter:submit
Tab:next answer  |  Esc:scrollback  |  Shift+x:dismiss
";
    /// 6d5e74ab 用的 w168:pB7 真畫面（帶 402 那一句與縮排）。
    const PB7_SCREEN: &str = include_str!("fixtures/grok_limit_hit.txt");

    async fn grok_turn(screen: &str) -> (tt::Env, db::Bot, String, String) {
        let env = tt::env().await;
        let app = env.app.clone();
        let bot = tt::claude_bot(&app, &env.project_id, "gk").await;
        sqlx::query("UPDATE bots SET kind='grok' WHERE id=?").bind(&bot.id).execute(&app.db).await.unwrap();
        let bot = db::bot(&app.db, &bot.id).await.unwrap().unwrap();
        let run = tt::fake_run(&app, &bot.id).await;
        let turn = crate::lifecycle::run_state::a_turn(&app, &bot.id, Some(&run), "in_flight").await;
        env.herdr.set_screen(&format!("pane-{}", bot.id), screen);
        (env, bot, run, turn)
    }

    async fn status(app: &Arc<App>, turn: &str) -> String {
        crate::lifecycle::run_state::turn_status(app, turn).await
    }

    async fn wait_failed(app: &Arc<App>, turn: &str) -> String {
        for _ in 0..250 {
            let s = status(app, turn).await;
            if s != "in_flight" {
                return s;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        status(app, turn).await
    }

    fn no_keys(env: &tt::Env) {
        assert!(env.herdr.calls_to("pane.send_keys").is_empty() && env.herdr.calls_to("pane.send_text").is_empty(), "不准替使用者按任何選項（1、2 是付費）");
    }

    fn blocked_event(bot_id: &str) -> crate::herdr::Event {
        crate::herdr::Event {
            event: "pane_agent_status_changed".into(),
            data: serde_json::json!({"pane_id": format!("pane-{bot_id}"), "agent_status": "blocked"}),
        }
    }

    /// 撞限要記在 **grok** 那一格：6d5e74ab 讓 grok 走 `mark_codex_limit_hit`，而 `turn_error::record` 對它一律 `resolve_quota_base(.., "codex", ..)`，
    /// 於是 grok 撞週限卻把 codex 標成用盡（codex 的派工被擋），grok 自己那格沒事（派工照派）。
    #[tokio::test]
    async fn the_limit_lands_on_groks_quota_and_not_codexs() {
        for screen in [TICKET_SCREEN, PB7_SCREEN] {
            let (env, bot, run, _turn) = grok_turn(screen).await;
            let app = env.app.clone();
            capture_codex_usage_notices(&app, &bot.id, &run).await.unwrap();
            let quotas = app.quotas.lock().await;
            assert!(quotas.get("codex").is_none_or(|q| q.limit_hit.is_none() && q.five_hour.is_none()), "codex 那格不該被 grok 的撞限動到：{:?}", quotas.get("codex"));
            let grok = quotas.get("grok").expect("grok 那一格");
            assert!(grok.limit_hit.is_some(), "grok 那一格要標成撞限");
            assert_eq!(grok.seven_day.as_ref().map(|w| w.used_pct), Some(100.0), "grok 只有週窗，存在 seven_day");
            drop(quotas);
            assert!(crate::quota::try_limit_hit_for_bot(&app, &bot).await.unwrap().is_some(), "派工看得到 grok 已用完");
        }
    }

    /// herdr 判成 `working -> idle` 時的終端備援：不能把這張畫面當成 agent 的回覆收成 `completed_fallback`（那樣回合「成功」
    /// 了，派工不會 park、UI 只顯示「可能不完整」的一坨終端字）。
    #[tokio::test]
    async fn the_fallback_does_not_store_the_limit_screen_as_the_reply() {
        for screen in [TICKET_SCREEN, PB7_SCREEN] {
            let (env, bot, run, turn) = grok_turn(screen).await;
            let app = env.app.clone();
            assert!(try_fallback(&app, &run, Some(&turn)).await.unwrap());
            assert_eq!(status(&app, &turn).await, "failed", "撞限是失敗的回合，不是 completed_fallback");
            let replies: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM messages WHERE turn_id=? AND role='assistant'").bind(&turn).fetch_one(&app.db).await.unwrap();
            assert_eq!(replies, 0, "選單的字不是 agent 的回覆");
            assert!(crate::quota::try_limit_hit_for_bot(&app, &bot).await.unwrap().is_some(), "派工看得到 grok 已用完");
            no_keys(&env);
        }
    }

    /// 票上的現象：herdr 說 `blocked`，沒有 working->idle 那條邊，備援與掃描都不會跑——bot 就停在 blocked，回合在飛。
    #[tokio::test]
    async fn a_blocked_grok_on_the_limit_screen_is_marked_exhausted() {
        for screen in [TICKET_SCREEN, PB7_SCREEN] {
            let (env, bot, _run, turn) = grok_turn(screen).await;
            let app = env.app.clone();
            crate::events::handle_status(&app, crate::config::LOCAL_HOST, "test", &blocked_event(&bot.id)).await;
            assert_eq!(wait_failed(&app, &turn).await, "failed", "blocked 那條邊要看畫面");
            assert!(crate::quota::try_limit_hit_for_bot(&app, &bot).await.unwrap().is_some());
            no_keys(&env);
        }
    }

    /// blocked 邊漏了（daemon 重啟時 bot 已經停在那裡，不會再有邊）：回合進行中的 poller 是安全網，`blocked` 時它整個跳過。
    #[tokio::test]
    async fn the_progress_poller_reads_a_blocked_limit_screen_too() {
        let (env, bot, run, turn) = grok_turn(TICKET_SCREEN).await;
        let app = env.app.clone();
        sqlx::query("UPDATE runs SET agent_status='blocked' WHERE id=?").bind(&run).execute(&app.db).await.unwrap();
        arm_progress(&app, &run, &bot.id, &turn).await;
        assert_eq!(wait_failed(&app, &turn).await, "failed");
        assert!(crate::quota::try_limit_hit_for_bot(&app, &bot).await.unwrap().is_some());
        no_keys(&env);
    }

    const WEEKLY: &str = "You hit your weekly limit.";
    const BALANCE: &str = "Turn failed: Request failed (402): Grok Build usage balance exhausted";

    /// 同一張畫面的兩句（402 credits 用完、週限）不管誰先來，撞限都是週限的那個窗與 7 天保底：後到的不能把它縮成 5 小時；
    /// 同一句重複看到，撞的那一刻不往後推。
    #[tokio::test]
    async fn the_two_lines_of_one_screen_never_shorten_each_other() {
        let at = |t: &Option<String>| chrono::DateTime::parse_from_rfc3339(t.as_deref().unwrap()).unwrap();
        for order in [[WEEKLY, BALANCE], [BALANCE, WEEKLY]] {
            let (env, bot, _run, _turn) = grok_turn(TICKET_SCREEN).await;
            let app = env.app.clone();
            for line in order {
                crate::turn_error::mark_codex_limit_hit(&app, &bot, line).await.unwrap();
            }
            let hit = crate::quota::try_limit_hit_for_bot(&app, &bot).await.unwrap().unwrap();
            assert_eq!(hit.bucket.as_deref(), Some("seven_day"), "{order:?}");
            assert_eq!(hit.message, WEEKLY, "{order:?}");
            assert!(at(&hit.until) > chrono::Utc::now() + chrono::Duration::days(6), "{order:?}：7 天保底，不是 5 小時");
            assert_eq!(app.quotas.lock().await.get("grok").and_then(|q| q.seven_day.as_ref()).map(|w| w.used_pct), Some(100.0));

            crate::turn_error::mark_codex_limit_hit(&app, &bot, WEEKLY).await.unwrap();
            let again = crate::quota::try_limit_hit_for_bot(&app, &bot).await.unwrap().unwrap();
            assert_eq!((again.at, again.until), (hit.at, hit.until), "同一句再看到不重算");
        }
    }

    /// 只有 402（credits 用完、沒有窗）：撞限照記在 grok 那一格、擋派工，但不亂標窗、保底只有 5 小時。
    #[tokio::test]
    async fn a_balance_only_screen_blocks_grok_without_inventing_a_window() {
        let (env, bot, _run, _turn) = grok_turn(TICKET_SCREEN).await;
        let app = env.app.clone();
        crate::turn_error::mark_codex_limit_hit(&app, &bot, BALANCE).await.unwrap();
        let hit = crate::quota::try_limit_hit_for_bot(&app, &bot).await.unwrap().expect("擋派工");
        assert_eq!(hit.bucket, None);
        let until = chrono::DateTime::parse_from_rfc3339(hit.until.as_deref().unwrap()).unwrap();
        assert!(until < chrono::Utc::now() + chrono::Duration::hours(6), "沒有窗：保底只有 5 小時，不擋七天");
        let quotas = app.quotas.lock().await;
        let q = quotas.get("grok").unwrap();
        assert!(q.five_hour.is_none() && q.seven_day.is_none(), "沒有窗就不標窗");
        assert!(quotas.get("codex").is_none());
    }

    /// 算不準落在哪把 key（身分表還沒偵測完）：不猜 `grok:<身分>` 寫下去，欠著、照擋——跟 claude／codex 同一條規則（#198）。
    #[tokio::test]
    async fn a_grok_limit_whose_key_is_unknown_is_owed() {
        let (env, bot, run, turn) = grok_turn(TICKET_SCREEN).await;
        let app = env.app.clone();
        sqlx::query("UPDATE bots SET identity='gk0' WHERE id=?").bind(&bot.id).execute(&app.db).await.unwrap();
        let bot = db::bot(&app.db, &bot.id).await.unwrap().unwrap();
        assert!(capture_codex_usage_notices(&app, &bot.id, &run).await.is_err(), "記不進去回錯");
        assert_eq!(status(&app, &turn).await, "failed", "回合照樣收");
        assert!(app.quotas.lock().await.values().all(|q| q.limit_hit.is_none()), "沒有猜一格寫下去");
        assert!(crate::quota::try_limit_hit_for_bot(&app, &bot).await.unwrap().is_some(), "欠著照擋");
    }

    /// 別的 CLI 的 bot 停在 blocked、畫面上剛好有同一句：不動（這是 grok 的畫面）。
    #[tokio::test]
    async fn a_claude_bot_showing_the_same_words_is_left_alone() {
        let (env, bot, _run, turn) = grok_turn(TICKET_SCREEN).await;
        let app = env.app.clone();
        sqlx::query("UPDATE bots SET kind='claude' WHERE id=?").bind(&bot.id).execute(&app.db).await.unwrap();
        crate::events::handle_status(&app, crate::config::LOCAL_HOST, "test", &blocked_event(&bot.id)).await;
        tokio::time::sleep(Duration::from_millis(900)).await;
        assert_eq!(status(&app, &turn).await, "in_flight");
        assert!(app.quotas.lock().await.is_empty());
    }

    /// 票上要求：同一選單的其他變體（session／daily limit）也要認。6d5e74ab 的辨識只有 `weekly` 與 402 兩句。
    #[test]
    fn the_other_limit_variants_the_ticket_names_are_read_too() {
        for (title, window) in [
            ("You hit your session limit.", (Some("five_hour"), 5)),
            ("You hit your 5-hour limit.", (Some("five_hour"), 5)),
            ("You hit your daily limit.", (None, 24)),
            ("You hit your monthly limit.", (None, 5)),
            ("You've hit your weekly limit.", (Some("seven_day"), 168)),
        ] {
            let screen = TICKET_SCREEN.replace("You hit your weekly limit.", title);
            let lines = grok_limit_notice_lines(&screen);
            assert_eq!(lines, vec![title.to_string()], "{title}");
            assert_eq!(grok_limit_window(&lines[0]), window, "{title}");
        }
        // 只認畫面上那一句標題：回覆裡談到、或標題後面接了字母的不算。
        assert!(grok_limit_hit_line("Then you hit your daily limit and stopped").is_none());
        assert!(grok_limit_hit_line("You hit your daily limits, apparently").is_none());
        assert!(grok_limit_hit_line("You hit your limit").is_none(), "沒有說是哪個窗");
    }

    #[test]
    fn the_window_a_grok_line_names() {
        assert_eq!(grok_limit_window("┃  You hit your weekly limit."), (Some("seven_day"), 168));
        assert_eq!(grok_limit_window("You hit your session limit."), (Some("five_hour"), 5));
        assert_eq!(grok_limit_window(BALANCE), (None, 5));
        // 有窗的那句排前面：一次讀畫面只有第一句算新的。
        let lines = grok_limit_notice_lines(PB7_SCREEN);
        assert_eq!(lines.first().map(String::as_str), Some(WEEKLY), "{lines:?}");
        assert!(lines.iter().any(|l| l.contains("usage balance exhausted")));
    }

    /// 撞限要有出口：grok 沒有 Stop hook 之外的「額度回來了」訊號，橫幅又沒寫時間。`/usage` 探測回來說這個窗是撞限**之後**才開的
    /// （已經重置），撞限要作廢，不能擋到重啟 daemon。
    #[tokio::test]
    async fn the_limit_ends_when_a_fresh_usage_reading_says_the_window_reset() {
        let (env, bot, run, _turn) = grok_turn(TICKET_SCREEN).await;
        let app = env.app.clone();
        capture_codex_usage_notices(&app, &bot.id, &run).await.unwrap();
        let hit = crate::quota::try_limit_hit_for_bot(&app, &bot).await.unwrap().expect("前提：撞限記下了（6d5e74ab 的掃描）");
        assert_eq!(hit.bucket.as_deref(), Some("seven_day"), "撞限帶額度窗，`/usage` 探測才校正得到");
        let reading = crate::quota::Quota {
            five_hour: None,
            seven_day: Some(crate::quota::Window { used_pct: 10.0, resets_at: Some(db::iso_at(chrono::Utc::now() + chrono::Duration::days(8))) }),
            fable: None,
            reset_credits: None,
            limit_hit: None,
            plan: Some("SuperGrok".into()),
            updated_at: db::now(),
            source: "grok-usage".into(),
            account: None,
            host: crate::config::LOCAL_HOST.into(),
        };
        crate::quota::set(&app, crate::config::LOCAL_HOST, "grok", reading).await;
        assert!(crate::quota::try_limit_hit_for_bot(&app, &bot).await.unwrap().is_none(), "窗已經重置：撞限作廢");
    }
}
