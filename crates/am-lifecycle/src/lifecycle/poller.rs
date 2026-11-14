//! The terminal-side safety net for a turn: progress, stall, fallback and hookless capture.
//!
//! `arm_progress`, `pane_awaits_input`, `arm_fallback`/`try_fallback` and the hookless capture are
//! one chain with `hookrecv`'s late-hook write-back (a turn closed here with no reply must still be
//! fillable by a hook that arrives afterwards); they are kept together on purpose.

#[cfg(all(test, feature = "daemon-test-harness"))]
use crate::state::App;
use super::*;

#[path = "observed_status.rs"]
mod observed_status;

/// Observe one Claude menu, or discard receipts for runs no longer active.
pub async fn observe_claude_menu(app: &impl LcHost, run: Option<&db::Run>, active: &[String]) -> anyhow::Result<bool> {
    observed_status::retain_menu_receipts(active);
    match run {
        Some(run) => observed_status::observe_menu(app, run).await,
        None => Ok(false),
    }
}

/// Safety net for a run left at `working` after its turn ended (see `observed_status::settle_stale_working`).
pub async fn settle_stale_working(app: &impl LcHost, run: &db::Run) -> anyhow::Result<bool> {
    observed_status::settle_stale_working(app, run).await
}

/// herdr 報 blocked、畫面上卻沒有可回答的提示：改掉狀態（`observed_status::settle_false_blocked`）。回 `true`＝這次改了。
pub async fn settle_false_blocked(app: &impl LcHost, run: &db::Run) -> anyhow::Result<bool> {
    observed_status::settle_false_blocked(app, run).await
}

/// 上一個的反向：之前判成誤報的 run，畫面出現提示就改回 blocked（`observed_status::reblock_if_prompt`）。
pub async fn reblock_if_prompt(app: &impl LcHost, run: &db::Run) -> anyhow::Result<bool> {
    observed_status::reblock_if_prompt(app, run).await
}

#[path = "codex_session_refresh.rs"]
mod codex_session_refresh;

/// codex run 還沒綁 native session 時（沒有回合也一樣）向 herdr 補綁；綁到回新的 run，沒綁到／節流中回 `None`。
pub async fn bind_codex_session_if_missing(app: &impl LcHost, run: &db::Run) -> Option<db::Run> {
    codex_session_refresh::refresh_idle(app, run).await
}


/// Unchanged polls at an empty composer before we complete the Turn ourselves (~14s; a slow hook still wins).
pub const IDLE_POLLS: u32 = 20;

/// 這回合畫面從沒出現過任何東西時的門檻（~63s）。2026-09-13（GROK、w168:pN）：grok 還沒印第一個字，
/// 空 `❯` 被當成等輸入，備援關掉回合，36 秒後 Stop hook 撞上已關的回合。等久只是備援晚接手；
/// 等不夠會吃掉使用者的問題。
pub const IDLE_POLLS_SILENT: u32 = 90;

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
async fn flush_progress(app: &(impl crate::capabilities::Emit + crate::lifecycle::poller::ProgressEmitted), run_id: &str, pending: &mut Option<Value>, force: bool) {
    let Some(frame) = pending.take() else { return };
    let mut emitted = app.progress_emitted().lock().await;
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

#[cfg(not(all(test, feature = "daemon-test-harness")))]
fn progress_interval(_run_id: &str) -> Duration {
    PROGRESS_INTERVAL
}

/// 測試可以把單一 run 的輪詢調快（[`progress_poll_tests`]），別的測試的 poller 照舊。
#[cfg(all(test, feature = "daemon-test-harness"))]
fn progress_interval(run_id: &str) -> Duration {
    progress_poll_tests::interval(run_id).unwrap_or(PROGRESS_INTERVAL)
}

/// 這一輪還是不是這一回合的 poller：讀到了、確定不是（回合收掉或換了一筆、run 或 bot 沒了）回 `Ok(None)`；
/// 讀不到回錯（#193）。bot 與送了什麼讀到一次就留著。
async fn still_ours(
    app: &impl crate::capabilities::Db,
    run_id: &str,
    bot_id: &str,
    turn_id: &str,
    bot: &mut Option<db::Bot>,
    sent: &mut Option<Vec<String>>,
) -> anyhow::Result<Option<db::Run>> {
    match db::in_flight_turn(app.db(), run_id).await? {
        Some(t) if t.id == turn_id => {}
        _ => return Ok(None),
    }
    let Some(run) = db::run(app.db(), run_id).await? else { return Ok(None) };
    if bot.is_none() {
        let Some(b) = db::bot(app.db(), bot_id).await? else { return Ok(None) };
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
pub async fn arm_progress(app: &impl LcHost, run_id: &str, bot_id: &str, turn_id: &str) {
    // A new turn is starting: whatever cut the *previous* one short is history (§4.3a). 清不掉就在 poller 裡
    // 再清：留著的話，這一回合斷在同一句錯誤上會被「同一則只記一次」吞掉（#193）。
    let mut error_cleared = crate::turn_error::clear(app, run_id, bot_id).await.is_ok();
    let mut pollers = app.progress_pollers().lock().await;
    if let Some(h) = pollers.remove(run_id) {
        h.abort();
    }
    let app2 = (*app).clone();
    let run_id = run_id.to_string();
    let bot_id = bot_id.to_string();
    let turn_id = turn_id.to_string();
    let key = run_id.clone();
    let h = tokio::spawn(Box::pin(async move {
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
            #[cfg(all(test, feature = "daemon-test-harness"))]
            super::race_point::hit("progress_poll", &run_id).await;
            // 讀不到（DB 一時 busy／I/O error）不是「回合收掉了」：延後重讀，不放掉這一回合（#193）。
            let run = match still_ours(&app2, &run_id, &bot_id, &turn_id, &mut bot, &mut sent).await {
                Ok(Some(run)) => run,
                Ok(None) => match observed_status::finish(&app2, &run_id, &turn_id).await {
                    Ok(true) => break,
                    // Stop may beat the idle redraw. Keep the existing poller/backoff until
                    // its own receipt can be restored, or a successor turn takes the pane.
                    Ok(false) => { failures = 0; continue; }
                    Err(e) => {
                        failures = poll_unreadable(&turn_id, failures, &e);
                        continue;
                    }
                },
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
            let stamp = if observed_status::needed(&run, &turn_id) {
                observed_status::Stamp::capture(&app2, &run).await.ok()
            } else { None };
            let Ok(read) = client.pane_read(&pane, "recent_unwrapped", 160).await else { continue };
            if let Some(stamp) = stamp {
                if let Err(e) = observed_status::observe(&app2, &client, &stamp, &turn_id, &bot.kind, &read.text).await {
                    tracing::debug!(run = %run_id, turn = %turn_id, error = %e, "could not reconcile observed pane status");
                }
            }
            let live = live_reply(&bot.kind, &read.text).unwrap_or_default();
            let live = sent.iter().fold(live, |acc, p| strip_echoed_prompt(&acc, p));
            // `clean_screen` drops the spinner row, so a thinking / tool phase needs its own `activity` field.
            let activity = live_activity(&bot.kind, &read.text).unwrap_or_default();
            let alert = live_alert(&bot.kind, &read.text).unwrap_or_default();
            // 壓縮中的畫面備援（`compacting.rs`，#1770）：spinner 還在就標記，畫面沒了只收掉自己標的（hook 來的等 hook 收）。
            if am_base::compacting::observe_screen(&run_id, am_base::compacting::screen_shows(&read.text), chrono::Utc::now()) {
                crate::capabilities::BotStatusEmit::emit_bot_status(&app2, &bot_id).await;
            }
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
            let agent_status = match db::run(app2.db(), &run_id).await {
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
        if let Err(e) = observed_status::finish(&app2, &run_id, &turn_id).await {
            tracing::debug!(run = %run_id, turn = %turn_id, error = %e, "could not restore observed pane status");
        }
        flush_progress(&app2, &run_id, &mut pending, true).await;
        app2.progress_pollers().lock().await.remove(&run_id);
    }));
    pollers.insert(key, h);
}


/// The user typed straight into the pane: open the `external` turn on the `-> working` edge so it
/// gets the same live bubble / progress as a web prompt. `delivery='ok'` so the §4.3 fallback
/// (which ignores other values) can still close it if the Stop hook never comes.
pub async fn begin_external_turn(app: &impl LcHost, run: &db::Run) {
    let lock = app.bot_lock(&run.bot_id).await;
    let _g = lock.lock().await;
    // Re-read under the lock: `prompt()` may have opened a turn, and the watcher is armed before
    // the run is `running`, so a boot-time `-> working` blip is not the user typing.
    let Ok(Some(run)) = db::run(app.db(), &run.id).await else { return };
    if run.state != "running" {
        return;
    }
    if !matches!(db::in_flight_turn(app.db(), &run.id).await, Ok(None)) {
        return;
    }
    let conv = match db::conversation_id(app.db(), &run.bot_id).await {
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
    .execute(app.db())
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
    let by_the_cli = match db::bot(app.db(), &run.bot_id).await {
        Ok(Some(bot)) => super::transcript_origin::started_by_the_cli_itself(app, &bot, run.transcript_path.as_deref()).await,
        _ => false,
    };
    let echo = if by_the_cli { None } else { pane_prompt_echo(app, &run).await };
    if by_the_cli {
        tracing::info!(turn = %tid, "external turn started by the CLI itself (task notification); no user message");
    }
    // 判不出是不是 CLI 自己起頭的（遠端 bot 的 transcript 在那台，這裡讀不到）時，畫面上最後一則回音可能就是上一則
    // 已經回答過的 prompt（2026-10-01 wits-ops-web：「去我的ego 開新身份」又出現一次）。跟它一樣就不存。
    let echo = match echo {
        Some(text) => match app.db().acquire().await {
            Ok(mut conn) => match crate::hookrecv::repeats_answered_prompt(&mut conn, &conv, &tid, &text).await {
                Ok(true) => {
                    tracing::info!(turn = %tid, "external turn: the echo on screen is the previous, answered prompt; not storing it again");
                    None
                }
                _ => Some(text),
            },
            Err(_) => Some(text),
        },
        None => None,
    };
    if let Some(text) = echo {
        if let Err(e) = insert_message(app, &conv, Some(&tid), "user", &text, "hook", false, None).await {
            tracing::warn!(turn = %tid, error = ?e, "external prompt echo not stored");
        }
    }
    emit_turn(app, &tid).await;
    arm_progress(app, &run.id, &run.bot_id, &tid).await;
}

async fn pane_prompt_echo(app: &(impl crate::capabilities::Db + crate::capabilities::HerdrRoutes + crate::hosts::HostsAccess), run: &db::Run) -> Option<String> {
    let bot = db::bot(app.db(), &run.bot_id).await.ok().flatten()?;
    let pane = run.pane_id.clone()?;
    let client = client_for_run(app, run).await.ok()?;
    let read = client.pane_read(&pane, "recent_unwrapped", 160).await.ok()?;
    last_prompt_echo_text(&bot.kind, &read.text)
}

/// Everything printed since the prompt echo, chrome and reply markers (`⏺ ` / `• `) removed.
pub fn live_reply(kind: &str, text: &str) -> Option<String> {
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

pub const ACTIVITY_MAX: usize = crate::capture::ACTIVITY_MAX;

pub use am_base::composer_parse::pane_awaits_input;
/// 空 composer 靜止幾輪才算等輸入。herdr 說閒著且畫面印過東西 → 14 秒（2026-09-06 grok 卡 working
/// 的安全網）；herdr 說 working 或這回合什麼都沒印過 → 63 秒（2026-09-13 GROK 還在想就被關）。
pub fn idle_threshold(said_something: bool, agent_status: &str) -> u32 {
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
pub async fn turn_echo_texts(app: &impl crate::capabilities::Db, turn_id: &str) -> anyhow::Result<Vec<String>> {
    let rows = db::turn_user_messages_with_attachments(app.db(), turn_id).await?;
    let mut out = Vec::new();
    let delivered: Option<String> = sqlx::query_scalar("SELECT prompt_text FROM turns WHERE id = ?")
        .bind(turn_id)
        .fetch_optional(app.db())
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
pub fn screen_reply(kind: &str, text: &str, sent: &[String]) -> Option<String> {
    let raw = extract_reply(kind, text).or_else(|| clean_screen(kind, text))?;
    let stripped = sent.iter().fold(raw, |acc, p| strip_echoed_prompt(&acc, p));
    let stripped = stripped.trim();
    if stripped.is_empty() {
        None
    } else {
        Some(stripped.to_string())
    }
}

/// Only attach a Codex failure/interruption snapshot to this turn when its visible prompt echo
/// matches one of the exact forms we sent. Without this guard, a clipped or missing echo could
/// make an older assistant message look like the current partial reply.
pub fn codex_partial_reply(text: &str, sent: &[String]) -> Option<String> {
    let echo = last_prompt_echo_text("codex", text)?;
    if !sent.iter().any(|prompt| prompt.trim() == echo.trim()) {
        return None;
    }
    screen_reply("codex", text, sent)
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
pub fn is_clipped_echo(line: &str, rest: &[&str]) -> bool {
    let Some(body) = without_ellipsis(line.trim()) else { return false };
    let body = squash(body);
    if body.chars().count() < SQUASH_MIN {
        return false;
    }
    squash(&rest.join("")).starts_with(&body)
}

/// Drop lines 2..n of a multi-line prompt echo: `after_last_prompt_echo` only skips the `❯` row.
/// Matching against what we sent is exact, unlike guessing from indentation.
pub fn strip_echoed_prompt(text: &str, prompt: &str) -> String {
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
async fn pane_columns(app: &(impl crate::capabilities::Db + crate::capabilities::HerdrRoutes + crate::hosts::HostsAccess), run: &db::Run) -> Option<u32> {
    let pane = run.pane_id.clone()?;
    let client = client_for_run(app, run).await.ok()?;
    // 用 pane_id 問（#1454）：`pane.layout` 不認 workspace_id，回的是使用者正在看的那個分頁。
    client.pane_size(&pane).await.ok().flatten().map(|(w, _)| w)
}

/// Output shredded into single characters? herdr unwraps terminal wrapping, not the TUI's own
/// one-glyph-per-row layout, whose spaces are lost — say so rather than store a column.
pub fn is_shredded(text: &str) -> bool {
    crate::capture::is_shredded(text)
}

/// Spinner-row shape `<Verb>… (3m 18s · ↓ 11.0k tokens)`. Verb is random and glyphs change
/// across releases; only the bracketed time / token counter is stable.
pub fn is_activity_shape(s: &str) -> bool {
    crate::capture::is_activity_shape(s)
}

/// The spinner row of this turn, glyph stripped, for `turn_progress.activity` only — never stored,
/// so `clean_screen` / `extract_reply` can keep dropping it as chrome.
pub fn live_activity(kind: &str, text: &str) -> Option<String> {
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
/// `word` 在 `s` 裡以獨立的字出現：前後不是英數字也不是 `_`（所以 `stage_error`、`on_retry` 不算，`API error:` 算）。
fn has_word(s: &str, word: &str) -> bool {
    let is_word = |c: char| c.is_ascii_alphanumeric() || c == '_';
    s.match_indices(word).any(|(i, _)| {
        let before = s[..i].chars().next_back();
        let after = s[i + word.len()..].chars().next();
        !before.is_some_and(is_word) && !after.is_some_and(is_word)
    })
}

pub fn live_alert(kind: &str, text: &str) -> Option<String> {
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
        // `⎿` 是工具輸出的縮排（sed、cat 印出來的程式碼或 log），不是 CLI 自己說的話（#1686）。要在剝前導記號之前認。
        if s.starts_with('⎿') {
            continue;
        }
        // Drop a leading spinner / bullet glyph so `✻ API error …` matches too.
        let s = match s.chars().next() {
            Some(c) if !c.is_alphanumeric() => s[c.len_utf8()..].trim(),
            _ => s,
        };
        if s.is_empty() || s.chars().count() > ACTIVITY_MAX * 2 {
            continue;
        }
        let low = s.to_ascii_lowercase();
        // 要是獨立的字：`def test_otp_rejected_on_retry_still_propagates_stage_error():` 這種程式名稱（底線連在一起）
        // 同時含 retry 與 error，以前被當成重試橫幅掛在回覆下面（2026-10-03 console-rpa 用 heredoc 寫測試檔）。
        let says_error = has_word(&low, "error") || low.contains("錯誤") || has_word(&low, "overloaded");
        if !says_error {
            continue;
        }
        // Data, not a banner (2026-09-08): JSON or `|` rows from e.g. a sqlite dump.
        if s.contains('{') || s.contains('}') || s.matches('|').count() >= 2 {
            continue;
        }
        // 只認 CLI 自己印的橫幅：固定前綴（claude `API error`、codex `stream error`）。光有 error＋retry 字眼不夠，
        // `Err(e) => tracing::warn!(…, "…; retrying")` 這種程式碼行（#1686，2026-10-13 A-leader 的黃色橫幅）就是這樣被認成橫幅的。
        let banner = low.starts_with("api error") || low.starts_with("stream error");
        if !banner {
            continue;
        }
        let retrying = RETRY_TOKENS.iter().any(|t| if t.is_ascii() { has_word(&low, t) } else { low.contains(t) });
        // 談論 API error 處理的散文沒有重試字眼，也沒有 claude 的 `API Error:` 冒號。
        if !retrying && !low.starts_with("api error:") {
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


#[cfg(all(test, feature = "daemon-test-harness"))]
#[path = "poller_tests/nbsp_composer_tests.rs"]
mod nbsp_composer_tests;

const STALL_SECS: u64 = 12;

/// 期限（秒）。遠端 bot 的 hook 先躺在那台的 spool，要等下一輪掃描（[`crate::hookrecv::SCAN_EVERY`]）才進 `hook_events`，
/// 所以多等一輪；本機的 hook 直接寫進 DB，期限不變（issue #1769）。
pub fn stall_deadline_secs(remote: bool) -> u64 {
    if remote {
        STALL_SECS + crate::hookrecv::SCAN_EVERY.as_secs()
    } else {
        STALL_SECS
    }
}

/// 這顆 bot 的 stall 期限：看它的 host 是不是遠端。讀不到 host 當本機（期限最短，跟以前一樣）。
async fn stall_deadline_for(app: &impl LcHost, bot_id: &str) -> u64 {
    let remote = db::bot_host(app.db(), bot_id).await.is_ok_and(|h| h != crate::config::LOCAL_HOST);
    stall_deadline_secs(remote)
}

/// Rows above the bottom the composer can start (it grows with its text; claude adds a hint row).
pub const COMPOSER_TAIL: usize = 24;

/// Chars of our prompt head that must be visible in the composer. `contains`, not prefix: claude
/// puts `[Image #6]` in front of pasted text.
const COMPOSER_HEAD: usize = 12;

/// Never match on a fragment this short — a two-character prompt is in every screen.
const COMPOSER_HEAD_MIN: usize = 4;
#[allow(unused_imports)]
pub use crate::composer_parse::{
    composer_text, composer_text_whole, is_rule_row, undecorate_row,
};

/// Is our prompt still unsent in the box? 2026-09-07 11:21: claude swallowed the Enter while
/// compacting; the turn failed as a stall with the message one keystroke from sent.
///
/// 逐列比字（#1669）：框裡讀回的列要是原文的一段，列內的字與空白逐字相同；列與列之間的接縫不計。
/// 以前只比前 12 個字、用 `contains`，使用者改過尾段也會被當成我們那則、照樣按 Enter。
pub fn composer_holds_prompt(kind: &str, screen: &str, sent: &str) -> bool {
    let Some(box_text) = crate::composer_parse::composer_text_whole_within(kind, screen, super::delivery::typed_tail(sent)) else { return false };
    if is_our_folded_paste(kind, &box_text, sent) {
        return true;
    }
    if kind == "codex" {
        super::delivery::rows_spell_text(&box_text, sent)
    } else {
        rows_within(&box_text, sent)
    }
}

/// 框裡是不是摺起來的「我們送的那一則」（#1915）。送出後的補按判斷（`delivery::composer_still_holds`）與
/// 「字還在不在框裡」（`delivery::text_still_in_box`）共用這一個 helper，兩邊對摺疊的判斷不會分岔（#1984）。
pub(crate) fn box_shows_our_folded_paste(kind: &str, screen: &str, sent: &str) -> bool {
    crate::composer_parse::composer_text_whole_within(kind, screen, super::delivery::typed_tail(sent)).is_some_and(|box_text| is_our_folded_paste(kind, &box_text, sent))
}

/// 框的第一列前面可能掛著 claude 的 `[Image #N]`，那一段不是草稿的字。
fn strip_image_chip(row: &str) -> &str {
    row.strip_prefix("[Image #")
        .and_then(|rest| rest.split_once(']'))
        .filter(|(n, _)| !n.is_empty() && n.chars().all(|c| c.is_ascii_digit()))
        .map_or(row, |(_, after)| after.trim_start())
}

/// `shown` 的列（`\n` 接起來，可能只是原文的一段：框捲動、或 claude 只畫出尾端）是不是 `sent` 裡連續的一段。
/// 每一列要在 `sent` 裡緊接著上一列出現，列內逐字相同；多出來的字（使用者接著打的）對不上就不是。
/// 至少要對上 [`COMPOSER_HEAD_MIN`] 個字，免得兩三個字在哪個畫面都找得到。
fn rows_within(shown: &str, sent: &str) -> bool {
    let mut rows: Vec<&str> = shown.split('\n').map(str::trim).filter(|r| !r.is_empty()).collect();
    if let Some(first) = rows.first_mut() {
        *first = strip_image_chip(*first);
    }
    rows.retain(|r| !r.is_empty());
    let Some((head, tail)) = rows.split_first() else { return false };
    if rows.iter().map(|r| r.chars().filter(|c| !c.is_whitespace()).count()).sum::<usize>() < COMPOSER_HEAD_MIN {
        return false;
    }
    sent.match_indices(*head).any(|(at, _)| {
        let mut rest = &sent[at + head.len()..];
        tail.iter().all(|row| match rest.trim_start().strip_prefix(*row) {
            Some(after) => {
                rest = after;
                true
            }
            None => false,
        })
    })
}

/// 長段貼上摺起來以後，框裡看不到原文，上面的逐列比對永遠落空：補 Enter 不會發生、重送又因為框不空被擋，
/// 12 秒後直接判失敗。2026-09-19 AM-1-XH：子 agent v4 卡在 blocked 的通知就這樣停在父 bot 的輸入框。
/// claude 摺成 `[Pasted text #3 +10 lines]`（2026-09-19，`+N lines` 是換行數）；codex 0.164.0 摺成
/// `[Pasted Content 1752 chars]`（字數，含結尾換行；實機畫面見 `folded_paste_tests::CODEX_FOLDED_PASTE_SCREEN`）。
///
/// 框裡**只有**這一個摺起來的貼上，而且數字正好是我們送的長度，才算是我們的：使用者接著打的字、別段貼上、
/// 長度對不上的都不按 Enter。摺起來的字看不到，片段無從比對，只能比長度。
pub(crate) fn is_our_folded_paste(kind: &str, box_text: &str, sent: &str) -> bool {
    let ours = match kind {
        "claude" => folded_claude_lines(box_text).filter(|&lines| lines == sent.trim_end().matches('\n').count()),
        "codex" => folded_codex_chars(box_text).filter(|&chars| chars == sent.chars().count()),
        _ => None,
    };
    ours.is_some_and(|n| n > 0)
}

/// claude 摺起來的標籤 `[Pasted text #N +M lines]` 的 M（換行數）；框裡不是只有這一個標籤就 `None`。
fn folded_claude_lines(box_text: &str) -> Option<usize> {
    let rest = box_text.trim().strip_prefix("[Pasted text #")?.strip_suffix(" lines]")?;
    let (id, lines) = rest.split_once(" +")?;
    if id.is_empty() || !id.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    lines.parse().ok()
}

/// codex 摺起來的標籤 `[Pasted Content N chars]` 的 N（字數）；框裡不是只有這一個標籤就 `None`。
fn folded_codex_chars(box_text: &str) -> Option<usize> {
    let digits = box_text.trim().strip_prefix("[Pasted Content ")?.strip_suffix(" chars]")?;
    if digits.is_empty() || !digits.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    digits.parse().ok()
}

#[cfg(all(test, feature = "daemon-test-harness"))]
#[path = "poller_tests/folded_paste_tests.rs"]
pub(crate) mod folded_paste_tests;

/// Early check after delivery: enough for the box to draw, short of the full stall deadline.
const NUDGE_EARLY_SECS: u64 = 3;

/// After re-sending Enter, how long the agent gets to react before the turn is failed.
const NUDGE_GRACE_SECS: u64 = 8;

/// 判斷救不救得回來要的狀態讀不到時，stall watchdog 隔多久整輪再看一次（#193）。
const STALL_RECHECK: Duration = Duration::from_secs(10);

/// Press Enter if our prompt is still in the box and the agent idle. Every reason to do nothing is
/// `Ok(false)`. The DB unreadable is an error (#193): whether the text is still in the box is then
/// unknown, and the watchdog must not go on to fail the turn as if it weren't.
async fn nudge_unsent_prompt(app: &(impl crate::capabilities::Db + crate::capabilities::HerdrRoutes + crate::hosts::HostsAccess), run_id: &str, turn_id: &str, sent: &[String]) -> anyhow::Result<bool> {
    let Some(run) = db::run(app.db(), run_id).await? else { return Ok(false) };
    if run.agent_status == "working" || run.agent_status == "blocked" {
        return Ok(false);
    }
    if !matches!(db::in_flight_turn(app.db(), run_id).await?, Some(t) if t.id == turn_id && t.delivery == "ok") {
        return Ok(false);
    }
    let Some(bot) = db::bot(app.db(), &run.bot_id).await? else { return Ok(false) };
    let Some(pane) = run.pane_id.clone() else { return Ok(false) };
    let Ok(client) = client_for_run(app, &run).await else { return Ok(false) };
    let Ok(screen) = super::delivery::read_styled(&client, &pane, "visible", 80).await else { return Ok(false) };
    if !sent.iter().any(|p| composer_holds_prompt(&bot.kind, &screen, p)) {
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
async fn nudge_if_unsent(app: &(impl crate::capabilities::Db + crate::capabilities::HerdrRoutes + crate::hosts::HostsAccess), run_id: &str, turn_id: &str, sent: &mut Option<Vec<String>>) -> anyhow::Result<bool> {
    if sent.is_none() {
        *sent = Some(turn_echo_texts(app, turn_id).await?);
    }
    nudge_unsent_prompt(app, run_id, turn_id, sent.as_deref().unwrap_or_default()).await
}

/// How many times one turn may be re-delivered because the prompt never showed up on screen.
/// One: a second loss means something is really wrong with the pane, and repeating a prompt the
/// agent *did* get is worse than failing the turn.
pub const MAX_PROMPT_RESENDS: i64 = 1;

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
pub fn prompt_never_reached_screen(kind: &str, screen: &str, sent: &[String]) -> bool {
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
    /// 一個字都沒打，但退款寫不進去；下輪先補退款再認領。
    RefundOwed,
}

fn resend_unreadable(turn_id: &str, e: &anyhow::Error) -> Resend {
    tracing::warn!(turn = %turn_id, error = %e, "cannot tell whether a lost prompt should be re-delivered; not failing the turn on it");
    Resend::Unreadable
}

impl Resend {
    #[cfg(all(test, feature = "daemon-test-harness"))]
    fn sent(self) -> bool {
        self == Resend::Sent
    }
}

/// How long the stall watchdog waits before it retries a resend that was blocked before typing
/// (the box had a draft for a moment, the transcript could not be read yet).
const RESEND_RETRY_SECS: u64 = 3;

/// Re-deliver a prompt that never reached the pane, at most [`MAX_PROMPT_RESENDS`] times per turn.
/// Same guards as [`nudge_unsent_prompt`]; every reason to do nothing is `Skipped`.
async fn resend_lost_prompt(app: &impl LcHost, run_id: &str, turn_id: &str, sent: &[String]) -> Resend {
    // DB 讀不到是 `Unreadable`，不是 `Skipped`（#193）：`Skipped` 之後 watchdog 就判失敗了。
    let run = match db::run(app.db(), run_id).await {
        Ok(Some(run)) => run,
        Ok(None) => return Resend::Skipped,
        Err(e) => return resend_unreadable(turn_id, &e),
    };
    if run.agent_status == "working" || run.agent_status == "blocked" {
        return Resend::Skipped;
    }
    // 重送看 `auto_resend`，不看有沒有證據（AGM 2026-09-16）：打過字但證不明的那條路重送會重複派工，
    // 所以它是 0；`agent.prompt` 一樣沒有證據，但它沒送進去才會走到這裡，重送是安全的。
    match db::in_flight_turn(app.db(), run_id).await {
        Ok(Some(t)) if t.id == turn_id && t.delivery == "ok" && t.auto_resend != 0 => {}
        Ok(_) => return Resend::Skipped,
        Err(e) => return resend_unreadable(turn_id, &e),
    }
    let bot = match db::bot(app.db(), &run.bot_id).await {
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
    match db::claim_resend(app.db(), turn_id, MAX_PROMPT_RESENDS).await {
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
            if let Err(e) = db::refund_resend(app.db(), turn_id).await {
                resend_unreadable(turn_id, &e);
                return Resend::RefundOwed;
            }
            tracing::warn!(run_id, turn = %turn_id, reason, "re-delivery was not attempted; the resend budget is given back");
            Resend::Blocked { reason, retry }
        }
        Ok(Delivered::Unproven(why)) => {
            tracing::warn!(run_id, turn = %turn_id, reason = why, "re-delivery could not be proven either");
            unproven(app, run_id, &bot.id, turn_id, why).await
        }
        // `execute_delivery` 在打第一個字之前的失敗都是 `NotAttempted`，走到 `Err` 就是鍵可能已經送出去了。
        Err(e) => {
            tracing::warn!(run_id, turn = %turn_id, error = %e, "re-delivering a lost prompt failed after typing started");
            unproven(app, run_id, &bot.id, turn_id, "resend_error").await
        }
    }
}

/// 重送打過字、證明不了：記成 `delivery='unknown'`；寫不進去就欠著（`UnprovenOwed`，#193）。
async fn unproven(app: &impl LcHost, run_id: &str, bot_id: &str, turn_id: &str, why: &'static str) -> Resend {
    match mark_resend_unproven(app, run_id, bot_id, turn_id, why).await {
        Ok(_) => Resend::Unproven,
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
async fn mark_resend_unproven(app: &impl LcHost, run_id: &str, bot_id: &str, turn_id: &str, why: &str) -> anyhow::Result<bool> {
    let text = format!(
        "（畫面上完全沒有這則訊息，已自動重打一次，但證明不了有送出（{why}）：字可能還留在輸入框裡，也可能已經被收下、排在後面。\
         這一則先記成「送達不明」，不判失敗。請到終端看一下——還在框裡就按 Enter 或清掉；已經在跑就等它回完。）"
    );
    let res = async {
        let mut tx = app.db().begin().await?;
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
    let applied = res.is_some();
    if let Some(m) = res {
        emit_message_added(app, bot_id, m).await;
        emit_turn(app, turn_id).await;
    }
    if applied {
        super::late_proof::arm(app, run_id, bot_id, turn_id);
    }
    Ok(applied)
}

/// The watchdog's resend, with one retry when the first try was refused before typing anything.
///
/// Before this, a refunded budget was never used: `arm_stall` failed the turn right after the refusal,
/// and nothing re-arms a watchdog later (review2 deliv #4). `None` = the stall timer was replaced
/// meanwhile (the caller stops); otherwise `(what the last try came to, what blocked a try)`.
async fn resend_with_retry(
    app: &impl LcHost,
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
        if app.stall_timers().lock().await.get(run_id) != Some(&generation) {
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
            Resend::Skipped | Resend::Unproven | Resend::UnprovenOwed(_) | Resend::Unreadable | Resend::RefundOwed => return Some((last, blocked)),
        }
    }
    Some((last, blocked))
}

/// The agent must leave `idle` within the stall deadline (`stall_deadline_secs`) after delivery, or the Turn sits `in_flight`
/// forever (not logged in, invisible modal). Cancelled by the first `working` / `blocked` event.
pub async fn arm_stall(app: &impl LcHost, run_id: &str, bot_id: &str, turn_id: &str) {
    let deadline = stall_deadline_for(app, bot_id).await;
    let mut timers = app.stall_timers().lock().await;
    static NEXT_GENERATION: AtomicU64 = AtomicU64::new(0);
    let generation = NEXT_GENERATION.fetch_add(1, Ordering::Relaxed);
    timers.insert(run_id.to_string(), generation);
    let app2 = (*app).clone();
    let run_id = run_id.to_string();
    let bot_id = bot_id.to_string();
    let turn_id = turn_id.to_string();
    tokio::spawn(Box::pin(async move {
        let mut st = Stall::default();
    // Every prompt gets the early look: a single line pasted into a busy TUI loses its Enter too.
        tokio::time::sleep(Duration::from_secs(NUDGE_EARLY_SECS)).await;
        {
            let lock = app2.bot_lock(&bot_id).await;
            let _g = lock.lock().await;
            if app2.stall_timers().lock().await.get(&run_id) != Some(&generation) {
                return;
            }
            // 早看的這一眼讀不到就算了：期限那一輪會再看，那一輪讀不到才擋著不判失敗。
            match nudge_if_unsent(&app2, &run_id, &turn_id, &mut st.sent).await {
                Ok(n) => st.nudged = n,
                Err(e) => tracing::debug!(turn = %turn_id, error = %e, "early nudge could not read the turn; the deadline looks again"),
            }
        }
        tokio::time::sleep(Duration::from_secs(deadline - NUDGE_EARLY_SECS)).await;
        loop {
            match stall_round(&app2, &run_id, &bot_id, &turn_id, generation, &mut st, Duration::from_secs(NUDGE_GRACE_SECS)).await {
                StallRound::Replaced => return,
                StallRound::Unsure => tokio::time::sleep(STALL_RECHECK).await,
                StallRound::Done => break,
            }
        }
        let mut timers = app2.stall_timers().lock().await;
        if timers.get(&run_id) == Some(&generation) {
            timers.remove(&run_id);
        }
    }));
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
    owed_refund: bool,
}

#[derive(Debug, PartialEq, Eq)]
enum StallRound {
    /// 計時器被換掉（新的回合、取消）：停手，什麼都不動。
    Replaced,
    /// 有一步讀寫失敗、判斷不了（#193、#1487）：不判失敗，隔 [`STALL_RECHECK`] 整輪再看。
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
    app: &impl LcHost,
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
        if app.stall_timers().lock().await.get(run_id) != Some(&generation) {
            return StallRound::Replaced;
        }
        if st.owed_refund {
            if let Err(e) = db::refund_resend(app.db(), turn_id).await {
                stall_unsure(turn_id, &e);
                return StallRound::Unsure;
            }
            st.owed_refund = false;
        }
        match nudge_if_unsent(app, run_id, turn_id, &mut st.sent).await {
            Ok(n) => st.nudged |= n,
            Err(e) => unsure = stall_unsure(turn_id, &e),
        }
    }
    #[cfg(all(test, feature = "daemon-test-harness"))]
    super::race_point::hit("stall_after_nudge", turn_id).await;
    // Not in the box either: the prompt never reached the TUI. Deliver it again once instead of
    // failing the turn with a system message the user has to act on.
    let mut resent_now = false;
    if !st.nudged && !st.resent && !unsure {
        // 沒在框裡、畫面上也看不到：不一定是「沒送到」。Claude 長思考時畫面是 `✽ Orchestrating… (13s · ↓ 537 tokens)` 加一個空的 `❯`，
        // herdr 偵測又把它當 idle（沒有 `working` 事件），這時重送等於把同一則 prompt 再打一次（issue #956）。
        // 這一輪 hook 已經有動靜、或畫面上有進行中的 spinner／計時＝agent 收到了，不重送、也不判失敗。
        if agent_is_active(app, run_id, bot_id, turn_id).await {
            tracing::info!(turn = %turn_id, bot = %bot_id, "stall watchdog: the agent is already working on the prompt (hook activity or a live spinner); not resending");
            return StallRound::Done;
        }
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
            Resend::RefundOwed => {
                st.owed_refund = true;
                unsure = true;
            }
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
        if app.stall_timers().lock().await.get(run_id) != Some(&generation) {
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
    if app.stall_timers().lock().await.get(run_id) != Some(&generation) {
        return StallRound::Replaced;
    }
    if let Some(why) = st.owed_unproven {
        match mark_resend_unproven(app, run_id, bot_id, turn_id, why).await {
            Ok(_) => st.owed_unproven = None,
            Err(e) => unsure = stall_unsure(turn_id, &e),
        }
    }
    if unsure {
        return StallRound::Unsure;
    }
    if let Err(e) = fail_stalled_turn(app, run_id, bot_id, turn_id, st.nudged, st.resent, st.blocked).await {
        tracing::warn!(error = ?e, "stall watchdog failed");
        return StallRound::Unsure;
    }
    StallRound::Done
}

pub async fn cancel_stall(app: &impl crate::lifecycle::poller::StallTimers, run_id: &str) {
    app.stall_timers().lock().await.remove(run_id);
}

async fn fail_stalled_turn(
    app: &impl LcHost,
    run_id: &str,
    bot_id: &str,
    turn_id: &str,
    nudged: bool,
    resent: bool,
    resend_blocked: Option<&str>,
) -> anyhow::Result<()> {
    let Some(run) = db::run(app.db(), run_id).await? else { return Ok(()) };
    if run.agent_status == "working" || run.agent_status == "blocked" {
        return Ok(());
    }
    let Some(turn) = db::in_flight_turn(app.db(), run_id).await? else { return Ok(()) };
    if turn.id != turn_id || turn.delivery != "ok" {
        return Ok(());
    }
    // issue #956: 判 STALL 前先看 agent 有沒有在動——hook 有動靜（PostToolUse…）或畫面上有進行中的 spinner／計時，
    // 就是收到了在處理（長思考、編排），不是「沒反應」。herdr 對 Claude 2.1.x 的 Orchestrating 畫面報 idle，所以光看 `agent_status` 會誤判。
    if has_hook_activity(app.db(), bot_id, &turn.created_at, turn_id).await {
        tracing::info!(turn = %turn_id, bot = %bot_id, "prompt stall watchdog saw hook activity for this turn; not failing it");
        return Ok(());
    }
    // Codex: 判失敗前先看 rollout 是否已完成回覆（例如 task_complete）。
    // 若已有回覆，走 try_fallback 補收，不寫 stall 誤報。
    let bot_for_check = db::bot(app.db(), bot_id).await?;
    if bot_for_check.as_ref().is_some_and(|b| b.kind == "codex") {
        let sent = turn_echo_texts(app, turn_id).await.unwrap_or_default();
        let since_iso = super::codex_rollout::turn_since(&turn.created_at, turn.delivered_at.as_deref());
        let rollout_done = if let Some(bot) = bot_for_check.as_ref() {
            super::codex_rollout::codex_rollout_outcome_for_in_flight(app, bot, &run, &sent, &since_iso).await.is_some()
        } else {
            false
        };
        if rollout_done {
            tracing::info!(turn = %turn_id, bot = %bot_id, "prompt stall watchdog saw codex rollout task_complete; attempting try_fallback");
            if try_fallback(app, run_id, Some(turn_id)).await.unwrap_or(false) {
                return Ok(());
            }
        }
    }
    // Quote the screen, never assert a cause (the host may well be logged in).
    let mut snapshot: Option<String> = None;
    let mut hints: Vec<String> = Vec::new();
    if let Ok(client) = client_for_run(app, &run).await {
        if let Some(pane) = run.pane_id.as_deref() {
            if let Ok(read) = client.pane_read(pane, "visible", 60).await {
                if is_screen_busy(&read.text) {
                    tracing::info!(turn = %turn_id, bot = %bot_id, "prompt stall watchdog saw a live spinner on screen; not failing the turn");
                    return Ok(());
                }
                hints = stall_hint_lines(&read.text);
                snapshot = Some(read.text);
            }
        }
    }
    let mut reason = stall_reason(&hints, nudged, stall_deadline_for(app, bot_id).await);
    if resent {
        reason.push_str(&format!("\n（第一次送出後畫面上完全沒有這則訊息，已自動重送 {MAX_PROMPT_RESENDS} 次，仍沒有反應。）"));
    } else if let Some(why) = resend_blocked {
        // 以前這種情況的訊息完全不提試過重送：看起來像 agent 沒反應，其實是重送被擋下（review2 deliv #4）。
        reason.push_str(&format!("\n（畫面上完全沒有這則訊息；試著自動重送時被擋下（{why}），一個字都沒打，所以沒有送出。清掉擋住的東西後請重送。）"));
    }
    let mut tx = app.db().begin().await?;
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
pub fn stall_reason(hints: &[String], nudged: bool, secs: u64) -> String {
    let head = format!("agent 在 {secs} 秒內沒有對訊息作出反應。");
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

pub async fn abandon_turn(app: &impl LcHost, turn_id: &str) -> LcResult<()> {
    let conversation_id = sqlx::query_scalar::<_, String>("SELECT conversation_id FROM turns WHERE id=?")
        .bind(turn_id)
        .fetch_optional(app.db())
        .await
        .map_err(up)?
        .ok_or_else(|| LcError::NotFound("turn".into()))?;
    let bot_id = sqlx::query_scalar::<_, String>("SELECT bot_id FROM conversations WHERE id=?")
        .bind(&conversation_id)
        .fetch_one(app.db())
        .await
        .map_err(up)?;
    let lock = app.bot_lock(&bot_id).await;
    let _g = lock.lock().await;
    let t = sqlx::query_as::<_, db::Turn>("SELECT * FROM turns WHERE id=?")
        .bind(turn_id)
        .fetch_optional(app.db())
        .await
        .map_err(up)?
        .ok_or_else(|| LcError::NotFound("turn".into()))?;
    if t.status != "in_flight" {
        return Err(LcError::conflict("turn is neither in-flight nor of unknown delivery", json!({"turn_id": t.id})));
    }
    let res = super::turn_controller::fail(app.db(), turn_id, super::turn_controller::DeliveryOnFail::FailedIfUnknown, "使用者放棄這一回合")
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
pub async fn arm_fallback(app: &impl LcHost, run_id: &str, bot_id: &str) {
    arm_fallback_after(app, run_id, bot_id, FALLBACK_DELAY).await
}

async fn arm_fallback_after(app: &impl LcHost, run_id: &str, bot_id: &str, delay: Duration) {
    // 讀不到就不排：排了也不知道要收哪一回合，收錯比不收糟（這一回合有 poller 的閒置備援與 stuck watchdog 兜著）。
    let armed_for = match db::in_flight_turn(app.db(), run_id).await {
        Ok(turn) => turn.map(|t| t.id),
        Err(e) => {
            tracing::warn!(run_id, error = ?e, "cannot tell which turn the terminal fallback is for; not arming it");
            return;
        }
    };
    let mut timers = app.fallback_timers().lock().await;
    static NEXT_GENERATION: AtomicU64 = AtomicU64::new(0);
    let generation = NEXT_GENERATION.fetch_add(1, Ordering::Relaxed);
    timers.insert(run_id.to_string(), generation);
    let app2 = (*app).clone();
    let run_id = run_id.to_string();
    let bot_id = bot_id.to_string();
    tokio::spawn(async move {
        tokio::time::sleep(delay).await;
        let lock = app2.bot_lock(&bot_id).await;
        let _g = lock.lock().await;
        if app2.fallback_timers().lock().await.get(&run_id) != Some(&generation) {
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
        match crate::turn_error::capture(&app2, &bot_id, &run_id).await {
            Ok(true) => super::api_error_nudge::arm(&app2, &bot_id, &run_id).await,
            Ok(false) => {}
            Err(e) => tracing::debug!(error = ?e, "turn error capture failed"),
        }
        let mut timers = app2.fallback_timers().lock().await;
        if timers.get(&run_id) == Some(&generation) {
            timers.remove(&run_id);
        }
    });
}

/// Read only this run's local rollout, using the same home/session binding as
/// delivery evidence. Unavailable or remote logs are not positive evidence.
async fn codex_compaction_pending(app: &impl LcHost, bot: &db::Bot, run: &db::Run, turn: &db::Turn) -> bool {
    if !db::project(app.db(), &bot.project_id).await.ok().flatten().is_some_and(|p| p.host == LOCAL_HOST) {
        return false;
    }
    let Some(session) = run.native_session_id.clone().filter(|s| !s.is_empty()) else { return false };
    let Some(home) = super::s6_ports::StuckTurnServices::codex_home(app, bot).await else { return false };
    // created_at is the enqueue time for queued turns, not this input's
    // delivery boundary. Missing delivery metadata is not positive evidence.
    let Some(since) = turn.delivered_at.clone() else { return false };
    tokio::task::spawn_blocking(move || {
        let path = super::delivery::codex_session_log(&home, &session)?;
        let log = crate::transcript_read::read_tail(&path, 4 * 1024 * 1024)?;
        Some(crate::compaction::codex_waiting_for_reply(&log, &since))
    }).await.ok().flatten().unwrap_or(false)
}

/// Close the in-flight turn from the pane (§4.3). `Ok(false)`: nothing to fall back on (no turn,
/// or untrusted delivery). A turn closed with zero reply can still be filled by hookrecv's late hook.
///
/// `expected_turn`：呼叫端排定（或輪詢）當下看的那一回合，`None`＝排定當下沒有回合在飛。到點時在飛的不是它（排定之後才開的新回合）就不收
/// ——畫面上最後一段回覆屬於上一回合，留給新回合自己的 hook／下一次 edge／poller（issue #216）。
pub(super) async fn try_fallback(app: &impl LcHost, run_id: &str, expected_turn: Option<&str>) -> anyhow::Result<bool> {
    let Some(run) = db::run(app.db(), run_id).await? else { return Ok(false) };
    let Some(turn) = db::in_flight_turn(app.db(), run_id).await? else { return Ok(false) };
    if expected_turn != Some(turn.id.as_str()) {
        tracing::debug!(run_id, turn = %turn.id, expected = ?expected_turn, "the turn in flight is not the one the fallback was armed for; leaving it");
        return Ok(false);
    }
    if turn.delivery != "ok" {
        return Ok(false);
    }
    let Some(bot) = db::bot(app.db(), &run.bot_id).await? else { return Ok(false) };
    let run = codex_session_refresh::refresh(app, &run, &bot, &turn.id).await.unwrap_or(run);
    // §12.5：沒有 hook 的 grok 先問它自己的對話檔，畫面只是備援。
    if let Some(done) = super::grok_transcript::settle_in_flight(app, &bot, &run, &turn).await {
        return Ok(done);
    }

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
    // 停在等人選的編號選單（權限框、Session paused…）：回合沒結束，選單那幾行也不是回覆（2026-09-25 cf-ox-fork-fork
    // 把 `2. Edit prompt and retry with …` 存成了回覆）。留在飛，等使用者選完由 hook／下一次 edge 收。
    if crate::tui_prompts::awaits_menu_choice(&read.text) || crate::tui_prompts::cold_resume_choice(&read.text).is_some() {
        tracing::debug!(run_id, "pane is waiting at a numbered menu; not completing the turn from it");
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

    // The recorded input is the only evidence for a Codex compact turn. In
    // particular, an empty external turn or compact-looking prose is not proof.
    let compact_turn = if bot.kind == "codex" && codex_hit.is_none() {
        let command = |s: &str| {
            let s = s.trim();
            s == "/compact" || s.starts_with("/compact ")
        };
        match turn.prompt_text.as_deref().filter(|s| !s.trim().is_empty()) {
            // prompt_text is the actual sent input; user bubbles can retain
            // routing mentions that were removed before delivery.
            Some(input) => command(input),
            None => {
                let inputs = db::turn_user_messages(app.db(), &turn.id).await?;
                !inputs.is_empty() && inputs.iter().all(|s| command(s))
            }
        }
    } else {
        false
    };

    // Auto-compaction can leave an idle-looking screen before the resumed
    // answer. Only independent rollout evidence permits stripping its cell.
    let compact_tail = if bot.kind == "codex" && !compact_turn && codex_hit.is_none() {
        match codex_compaction_tail(&fresh) {
            Some(tail) if codex_compaction_pending(app, &bot, &run, &turn).await => Some(tail),
            _ => None,
        }
    } else {
        None
    };
    let reply_screen = compact_tail.unwrap_or(&fresh);

    let codex_rollout_outcome = if bot.kind == "codex" && codex_hit.is_none() && !compact_turn {
        let sent = turn_echo_texts(app, &turn.id).await?;
        let since_iso = super::codex_rollout::turn_since(&turn.created_at, turn.delivered_at.as_deref());
        super::codex_rollout::codex_rollout_outcome_for_in_flight(app, &bot, &run, &sent, &since_iso).await
    } else {
        None
    };
    let (codex_rollout_reply, codex_rollout_error) = match codex_rollout_outcome {
        Some(super::codex_rollout::NativeOutcome::Reply(reply)) => (Some(reply), None),
        Some(super::codex_rollout::NativeOutcome::Error { native_id, message, reply })
            if turn.native_turn_id.as_deref().is_none_or(|id| id == native_id)
                && turn.native_session_id.as_deref().is_none_or(|id| Some(id) == run.native_session_id.as_deref()) => (None, Some((native_id, message, reply))),
        Some(super::codex_rollout::NativeOutcome::Error { .. }) => (None, None),
        None => (None, None),
    };

    // Derive the reply before SQLite's write lock (`pane_columns` RPC, `turn_echo_texts` other conn).
    let reply = if codex_hit.is_some() || codex_rollout_error.is_some() || compact_turn {
        None
    } else if let Some(r) = codex_rollout_reply.clone() {
        Some(r)
    } else {
    // Only the `❯` row counts as echo; lines 2..n would be stored as the answer. 讀不到送了什麼就不收（#193），
    // 回合留在飛、下一次再來：當成什麼都沒送，我們自己的 prompt 就被當成 agent 的回覆存下來。
        let sent = turn_echo_texts(app, &turn.id).await?;
        #[cfg(all(test, feature = "daemon-test-harness"))]
        super::race_point::hit("fallback_after_echo", &turn.id).await;
        // 回音看得到就以它為準（#1977）：游標可能停在回覆中途，切掉前面的字。compact 尾段照舊。
        let anchored = if compact_tail.is_none() { slice_after_sent_echo(&bot.kind, &read.text, &sent) } else { None };
        let screen = anchored.as_deref().unwrap_or(reply_screen);
        tracing::debug!(turn = %turn.id, branch = if anchored.is_some() { "echo" } else { "cursor" }, screen_len = screen.len(), fingerprint = %fingerprint(screen), "terminal fallback: reply window");
        match screen_reply(&bot.kind, screen, &sent).filter(|r| bot.kind != "grok" || !super::grok_transcript::is_startup_screen(r)) {
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

    if compact_tail.is_some() && reply.is_none() && codex_rollout_error.is_none() {
        // No cursor advance: every later idle poll must keep waiting until an
        // answer or hook arrives. The existing stuck-turn timeout remains active.
        return Ok(false);
    }

    // 沒有人送 prompt 的 external 回合（herdr 的 working→idle 抖一下就會開一個）：畫面上還是上一段回覆，游標又對不上
    // （尾端含 codex 狀態列的額度數字，一變就找不到）時，整個畫面被當成新內容，同一段回覆存第二次（am-lead 2026-09-25 10:36／11:27）。
    // 跟上一則 assistant 一樣就是同一段回覆，只收回合不存訊息；有人送過 prompt 的回合照舊（真的可能回一樣的話）。
    let reply = match reply {
        Some(r)
            if turn.origin == "external"
                && turn_echo_texts(app, &turn.id).await?.is_empty()
                && last_assistant_content(app, &turn.conversation_id).await?.is_some_and(|previous| {
                    if bot.kind == "codex" {
                        previous.split_whitespace().eq(r.split_whitespace())
                    } else {
                        previous.trim() == r.trim()
                    }
                }) =>
        {
            tracing::info!(turn = %turn.id, "terminal fallback of a prompt-less external turn re-read the previous reply; not storing it again");
            None
        }
        // claude 自己壓縮對話（#1759）：畫面只剩「Compacted while idle…」，不是回覆；說明由 SessionStart(compact) 寫。
        Some(r) if turn.origin == "external" && bot.kind == "claude" && crate::compaction::is_screen_notice(&r) && turn_echo_texts(app, &turn.id).await?.is_empty() => {
            tracing::info!(turn = %turn.id, "terminal fallback saw only a compaction notice; storing no reply");
            None
        }
        r => r,
    };

    let compact_note = if compact_turn {
        crate::compaction::codex_screen_note(
            app.db(), &bot.id, &turn.conversation_id, codex_compaction_duration(&fresh).unwrap_or(""),
        ).await?
    } else {
        None
    };

    // CAS claim + reply in one transaction, so a completed turn never lacks its reply.
    let is_codex_rollout_reply = codex_rollout_reply.is_some();
    let mut tx = app.db().begin().await?;
    // 撞限直接 `in_flight -> failed`：先落在 `completed_fallback` 再改 failed 不是合法邊，trigger 會把整個交易擋掉（#109）。
    let res = if codex_hit.is_some() || codex_rollout_error.is_some() {
        super::turn_controller::fail_on(&mut tx, &turn.id, super::turn_controller::DeliveryOnFail::Keep, "§4.3 備援看到撞限橫幅").await?
    } else if compact_turn {
        super::turn_controller::set_status_on(&mut tx, &turn.id, "in_flight", "completed", "Codex 壓縮完成，沒有 assistant 回覆").await?
    } else if is_codex_rollout_reply {
        super::turn_controller::set_status_on(&mut tx, &turn.id, "in_flight", "completed", "§4.3 Codex rollout 回覆").await?
    } else {
        super::turn_controller::set_status_on(&mut tx, &turn.id, "in_flight", "completed_fallback", "§4.3 終端快照備援").await?
    };
    if res != super::turn_controller::Outcome::Applied {
        return Ok(false);
    }
    tracing::info!(turn = %turn.id, "terminal fallback engaged");

    if let Some((native_id, error, partial_reply)) = codex_rollout_error {
        sqlx::query("UPDATE turns SET native_session_id=COALESCE(native_session_id, ?), native_turn_id=COALESCE(native_turn_id, ?) WHERE id=? AND run_id=?")
            .bind(&run.native_session_id).bind(&native_id).bind(&turn.id).bind(run_id).execute(&mut *tx).await?;
        sqlx::query("UPDATE runs SET turn_error=? WHERE id=? AND bot_id=?")
            .bind(&error).bind(run_id).bind(&bot.id).execute(&mut *tx).await?;
        let partial = if let Some(reply) = partial_reply {
            Some(insert_message_tx(&mut tx, &turn.conversation_id, Some(&turn.id), "assistant", &reply, "transcript", true, None).await?)
        } else { None };
        let message = insert_message_tx(&mut tx, &turn.conversation_id, Some(&turn.id), "system", &error, "system", true, None).await?;
        remember_pane_cursor_tx(&mut tx, run_id, &read).await?;
        tx.commit().await?;
        if let Some(message) = partial { emit_message_added(app, &bot.id, message).await; }
        emit_message_added(app, &bot.id, message).await;
        emit_turn(app, &turn.id).await;
        app.emit_bot_status(&bot.id).await;
        return Ok(true);
    }

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
        // 第二意見（#240）：備援先下手時，之後 `capture_codex_usage_notices` 看到的是「已寫過、回合也不在飛」而跳過，
        // 這一次命中就不會進帳本（#1467）。只記錄、鎖外問（spawn），回合與額度照下面的收。
        app.shadow_limit_hit(crate::judge::Sample {
            bot_id: bot.id.clone(),
            run_id: run_id.to_string(),
            project_id: bot.project_id.clone(),
            kind: bot.kind.clone(),
            matched_line: hit.clone(),
            screen: read.text.clone(),
        })
        .await;
        // 記在這顆 bot 的主機與身份那一格。讀不到主機不退回 `local`；記不進去就欠著，派送前與 flush 照欠著的那一筆擋、
        // 之後每次查詢先補寫（#198）。回合已經收掉了，照樣回 `true`。
        if let Err(e) = crate::turn_error::mark_codex_limit_hit(app, &bot, &hit).await {
            tracing::warn!(turn = %turn.id, error = %e, "codex limit hit owed; it holds this bot until it is recorded");
        }
        emit_turn(app, &turn.id).await;
        return Ok(true);
    }

    let compact_message = if let Some(note) = compact_note {
        Some(insert_message_tx(&mut tx, &turn.conversation_id, Some(&turn.id), "system", &note, "system", false, Some(&read.text)).await?)
    } else {
        None
    };
    let Some(reply) = reply else {
        // Only our own prompt: the turn is closed (composer unlocks); storing the echo would put words in the agent's mouth.
        tracing::info!(turn = %turn.id, "terminal fallback saw only our own prompt; storing no reply");
        remember_pane_cursor_tx(&mut tx, run_id, &read).await?;
        tx.commit().await?;
        if let Some(m) = compact_message {
            emit_message_added(app, &bot.id, m).await;
        }
        emit_turn(app, &turn.id).await;
        return Ok(true);
    };

    let is_codex_rollout_reply = codex_rollout_reply.is_some();
    let message_source = if is_codex_rollout_reply {
        "transcript"
    } else {
        "terminal_fallback"
    };
    let message_incomplete = !is_codex_rollout_reply;

    if is_codex_rollout_reply {
        tracing::info!(
            bot = %bot.name,
            turn = %turn.id,
            via = "rollout_fallback",
            len = reply.len(),
            hook_missing = true,
            "codex reply recovered from rollout"
        );
    }

    let message = insert_message_tx(
        &mut tx,
        &turn.conversation_id,
        Some(&turn.id),
        "assistant",
        &reply,
        message_source,
        message_incomplete,
        Some(&read.text),
    )
    .await?;
    remember_pane_cursor_tx(&mut tx, run_id, &read).await?;
    tx.commit().await?;
    if let Some(m) = compact_message {
        emit_message_added(app, &bot.id, m).await;
    }
    emit_message_added(app, &bot.id, message).await;
    emit_turn(app, &turn.id).await;
    Ok(true)
}

/// Spinner glyphs before an in-progress verb (`✢ Baking…`, `⠦ Thinking… 52s`); braille is codex/grok.
pub fn is_spinner_glyph(c: char) -> bool {
    crate::capture::claude::is_spinner_glyph(c)
}

/// Mid-turn: a spinner still on screen. codex's `• Working (4s • esc to interrupt)` has no ellipsis,
/// so it's matched separately (2026-09-08).
pub fn pane_still_busy(screen: &str) -> bool {
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

pub fn is_tool_progress(reply: &str) -> bool {
    crate::capture::claude::is_tool_progress(reply)
}

/// issue #956: 畫面上有進行中的 spinner／計時嗎。
/// - [`pane_still_busy`]：Claude／grok 的 spinner 列與 codex 的 `• Working (4s • esc to interrupt)`；
/// - [`is_activity_shape`]：任何 glyph、任何動詞的 `<Verb>… (13s · ↓ 537 tokens · thinking with high effort)`（Claude 2.1.x 的
///   `Orchestrating…` 之類，動詞會隨版本換，只有括號裡的經過時間／token 計數是穩定的）。
/// 已完成的行（`✻ Crunched for 9s`）沒有 `…`，不算。
pub fn is_screen_busy(screen: &str) -> bool {
    pane_still_busy(screen) || screen.lines().any(|l| is_activity_shape(l.trim()))
}

/// issue #956: 這個回合開始之後，這顆 bot 的 hook 有動靜嗎（收件匣有新事件，或這個回合已經有 hook 寫的訊息）。
/// 有＝agent 收到並在處理。`SessionStart` 不算：自動啟動的 bot 在回合建立之後、prompt 送達之前就會送它，證明不了 prompt 被收下。
pub async fn has_hook_activity(db: &sqlx::SqlitePool, bot_id: &str, since: &str, turn_id: &str) -> bool {
    let events: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM hook_events WHERE bot_id = ? AND received_at >= ? AND lower(body_json) NOT LIKE '%sessionstart%'",
    )
    .bind(bot_id)
    .bind(since)
    .fetch_one(db)
    .await
    .unwrap_or(0);
    if events > 0 {
        return true;
    }
    sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM messages WHERE turn_id = ? AND source = 'hook'")
        .bind(turn_id)
        .fetch_one(db)
        .await
        .unwrap_or(0)
        > 0
}

/// stall watchdog 判斷「agent 其實在忙這則 prompt」：hook 有動靜，或畫面上有進行中的 spinner／計時。讀不到就當作不是。
async fn agent_is_active(app: &impl LcHost, run_id: &str, bot_id: &str, turn_id: &str) -> bool {
    let Ok(Some(run)) = db::run(app.db(), run_id).await else { return false };
    let Ok(Some(turn)) = db::in_flight_turn(app.db(), run_id).await else { return false };
    if turn.id != turn_id {
        return false;
    }
    if has_hook_activity(app.db(), bot_id, &turn.created_at, turn_id).await {
        return true;
    }
    let Ok(client) = client_for_run(app, &run).await else { return false };
    let Some(pane) = run.pane_id.as_deref() else { return false };
    client.pane_read(pane, "visible", 60).await.is_ok_and(|read| is_screen_busy(&read.text))
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
pub(super) async fn capture_hookless_turn_locked(app: &impl LcHost, run_id: &str, seed: bool) -> anyhow::Result<bool> {
    let Some(run) = db::run(app.db(), run_id).await? else { return Ok(false) };
    let Some(bot) = db::bot(app.db(), &run.bot_id).await? else { return Ok(false) };
    if !is_hookless(&bot, &run) || run.state != "running" {
        return Ok(false);
    }
    // Like §4.3: a modal waiting for an answer is not an ended turn.
    if run.agent_status == "blocked" {
        return Ok(false);
    }
    // An in-flight turn belongs to `try_fallback`; capturing too would store it twice.
    if db::in_flight_turn(app.db(), run_id).await?.is_some() {
        return Ok(false);
    }
    // §12.5：grok 的對話檔讀得到就不刮畫面。
    if matches!(bot.kind.as_str(), "grok" | "agy" | "claude") {
        match super::grok_transcript::sync_locked(app, run_id).await {
            Ok(super::grok_transcript::Synced::Read { imported, .. }) => return Ok(imported > 0),
            Ok(super::grok_transcript::Synced::Unavailable) => {}
            Err(e) => tracing::warn!(run = %run_id, error = ?e, "grok transcript unreadable; falling back to the pane"),
        }
    }
    let Some(pane_id) = run.pane_id.clone() else { return Ok(false) };
    let conv = db::conversation_id(app.db(), &run.bot_id).await?;
    if seed && conversation_message_count(app, &conv).await? > 0 {
        return Ok(false);
    }
    let client = app
        .herdr_for_run(&run)
        .await
        .ok_or_else(|| anyhow::anyhow!("no Herdr session is available for run `{}`", run.id))?;
    let read = client.pane_read(&pane_id, "recent_unwrapped", 200).await?;
    // 同 `try_fallback`：停在編號選單上不是回合結束，選單不是回覆。
    if crate::tui_prompts::awaits_menu_choice(&read.text) || crate::tui_prompts::cold_resume_choice(&read.text).is_some() {
        return Ok(false);
    }
    let fresh = slice_after_cursor(&read.text, run.last_read_tail_hash.as_deref());
    // The pane echo is the only record of what was typed.
    let echo = last_prompt_echo_text(&bot.kind, &fresh);
    // Without a cursor, only the echo marks where the last turn began; with neither, just
    // remember the cursor — storing older turns as one message is worse than nothing.
    if run.last_read_tail_hash.is_none() && echo.is_none() {
        remember_pane_cursor(app, run_id, &read).await?;
        return Ok(false);
    }
    // 尋找此回合開始的時間下限（以此對話最新一筆 message 的時間或 run 的 started_at 為準）
    let since_iso = sqlx::query_scalar::<_, String>(
        "SELECT created_at FROM messages WHERE conversation_id=? ORDER BY created_at DESC LIMIT 1"
    )
    .bind(&conv)
    .fetch_optional(app.db())
    .await?
    .unwrap_or_else(|| run.started_at.clone());

    let codex_rollout_reply = if bot.kind == "codex" {
        super::codex_rollout::codex_rollout_reply_for_hookless(app, &bot, &run, &since_iso).await
    } else {
        None
    };

    let reply = if let Some(r) = codex_rollout_reply.as_ref() {
        r.clone()
    } else {
        let scraped = extract_reply(&bot.kind, &fresh).or_else(|| clean_screen(&bot.kind, &fresh));
        match (&echo, scraped) {
            (Some(p), Some(r)) => strip_echoed_prompt(&r, p),
            (None, Some(r)) => r,
            (_, None) => String::new(),
        }
    };
    // Unlike `try_fallback`, no turn waits to be closed, so no 「（終端沒有可辨識的回覆）」 bubble.
    // grok 的啟動選單不是回覆（§12.5）。
    if reply.trim().is_empty() || is_shredded(&reply) || (bot.kind == "grok" && super::grok_transcript::is_startup_screen(&reply)) {
        remember_pane_cursor(app, run_id, &read).await?;
        return Ok(false);
    }
    // herdr reports `working -> idle` more than once per answer: an identical reply is the same reply.
    if last_assistant_content(app, &conv).await?.as_deref().map(str::trim) == Some(reply.trim()) {
        remember_pane_cursor(app, run_id, &read).await?;
        return Ok(false);
    }
    let reply = truncate_hookless_reply(reply);

    let is_codex_rollout = codex_rollout_reply.is_some();
    let turn_status = if is_codex_rollout { "completed" } else { "completed_fallback" };
    let message_source = if is_codex_rollout { "transcript" } else { "terminal_fallback" };
    let message_incomplete = !is_codex_rollout;

    let tid = db::ulid();
    let now = db::now();

    if is_codex_rollout {
        tracing::info!(
            bot = %bot.name,
            turn = %tid,
            via = "rollout_fallback",
            len = reply.len(),
            hook_missing = true,
            "codex reply recovered from rollout"
        );
    }
    sqlx::query(
        "INSERT INTO turns (id, conversation_id, run_id, origin, status, delivery, created_at, completed_at)
         VALUES (?,?,?,'external',?,'ok',?,?)",
    )
    .bind(&tid)
    .bind(&conv)
    .bind(run_id)
    .bind(turn_status)
    .bind(&now)
    .bind(&now)
    .execute(app.db())
    .await?;
    if let Some(text) = echo.as_deref() {
        insert_message(app, &conv, Some(&tid), "user", text, "terminal_fallback", false, None).await?;
        // ULIDs only order by millisecond; wait so the reply doesn't sort above its prompt.
        tokio::time::sleep(Duration::from_millis(2)).await;
    }
    insert_message(app, &conv, Some(&tid), "assistant", &reply, message_source, message_incomplete, Some(&read.text)).await?;
    remember_pane_cursor(app, run_id, &read).await?;
    emit_turn(app, &tid).await;
    tracing::info!(run = %run_id, turn = %tid, bot = %bot.name, seed, "hookless turn captured from the pane");
    Ok(true)
}

/// [`capture_hookless_turn_locked`] for a caller that does not already hold the bot lock.
async fn capture_hookless_turn(app: &impl LcHost, run_id: &str, bot_id: &str, seed: bool) -> anyhow::Result<bool> {
    let lock = app.bot_lock(bot_id).await;
    let _g = lock.lock().await;
    capture_hookless_turn_locked(app, run_id, seed).await
}

/// Adopted hookless run (called by `reconcile`, off its task since both take the bot lock):
/// still `working` → open the `external` turn now; already `idle` → store the exchange once (`seed`).
pub fn spawn_adopted_capture(app: &impl LcHost, run_id: &str, bot_id: &str) {
    let (app, run_id, bot_id) = ((*app).clone(), run_id.to_string(), bot_id.to_string());
    tokio::spawn(async move {
        tokio::time::sleep(ADOPTED_CAPTURE_DELAY).await;
        let (Ok(Some(run)), Ok(Some(bot))) = (db::run(app.db(), &run_id).await, db::bot(app.db(), &bot_id).await) else {
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

/// Delay before reading the transcript at a `-> idle` edge: the CLI writes its last line a beat after herdr flips.
const IDLE_TRANSCRIPT_SYNC_DELAY: Duration = Duration::from_secs(2);

/// 沒有 hook 的 transcript run 轉成 idle：這一輪若沒有回合在飛（working 邊沒收到——adopt 時狀態不是 working、
/// herdr 一直回 unknown），完成就只能靠對話檔補（issue #1798）。只同步、不刮畫面；已經記過的問不重記。
/// 呼叫端拿著 bot 鎖；回傳有沒有新記下回合。
pub(super) async fn sync_idle_transcript_locked(app: &impl LcHost, run_id: &str) -> anyhow::Result<bool> {
    // 在飛的回合歸 `try_fallback`（它自己會問對話檔）。
    if db::in_flight_turn(app.db(), run_id).await?.is_some() {
        return Ok(false);
    }
    match super::grok_transcript::sync_locked(app, run_id).await? {
        super::grok_transcript::Synced::Read { imported, .. } => Ok(imported > 0),
        super::grok_transcript::Synced::Unavailable => Ok(false),
    }
}

/// [`sync_idle_transcript_locked`] off the status-event task: it waits a moment and takes the bot lock itself.
pub fn spawn_idle_transcript_sync(app: &impl LcHost, run_id: &str, bot_id: &str) {
    let (app, run_id, bot_id) = ((*app).clone(), run_id.to_string(), bot_id.to_string());
    tokio::spawn(async move {
        tokio::time::sleep(IDLE_TRANSCRIPT_SYNC_DELAY).await;
        let lock = app.bot_lock(&bot_id).await;
        let _g = lock.lock().await;
        if let Err(e) = sync_idle_transcript_locked(&app, &run_id).await {
            tracing::warn!(run = %run_id, error = ?e, "idle transcript sync failed");
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

#[cfg(all(test, feature = "daemon-test-harness"))]
#[path = "poller_tests/abandon_tests.rs"]
mod abandon_tests;

#[cfg(all(test, feature = "daemon-test-harness"))]
#[path = "poller_tests/hookless_capture_tests.rs"]
mod hookless_capture_tests;

#[cfg(all(test, feature = "daemon-test-harness"))]
#[path = "poller_tests/issue_17_tests.rs"]
mod issue_17_tests;

#[cfg(all(test, feature = "daemon-test-harness"))]
#[path = "poller_tests/progress_poll_tests.rs"]
mod progress_poll_tests;

#[cfg(all(test, feature = "daemon-test-harness"))]
#[path = "poller_tests/codex_limit_fallback_tests.rs"]
mod codex_limit_fallback_tests;

#[cfg(all(test, feature = "daemon-test-harness"))]
#[path = "poller_tests/codex_0155_fallback_tests.rs"]
mod codex_0155_fallback_tests;

#[cfg(all(test, feature = "daemon-test-harness"))]
#[path = "poller_tests/menu_fallback_tests.rs"]
mod menu_fallback_tests;

#[cfg(all(test, feature = "daemon-test-harness"))]
#[path = "poller_tests/progress_rate_tests.rs"]
mod progress_rate_tests;


#[cfg(all(test, feature = "daemon-test-harness"))]
#[path = "poller_tests/lost_prompt_tests.rs"]
mod lost_prompt_tests;

#[cfg(all(test, feature = "daemon-test-harness"))]
#[path = "poller_tests/fallback_binding_tests.rs"]
mod fallback_binding_tests;

#[cfg(all(test, feature = "daemon-test-harness"))]
#[path = "poller_tests/grok_limit_screen_tests.rs"]
mod grok_limit_screen_tests;

#[cfg(all(test, feature = "daemon-test-harness"))]
#[path = "poller_tests/codex_limit_quote_tests.rs"]
mod codex_limit_quote_tests;

/// 每個 run 的進度輪詢任務。（欄位在 `App`，由 composition 層 `app_ports_p0` 實作這個窄能力。）
#[allow(dead_code)]
pub trait ProgressPollers: Send + Sync {
    fn progress_pollers(&self) -> &tokio::sync::Mutex<std::collections::HashMap<String, tokio::task::JoinHandle<()>>>;
}

/// run 的卡住計時器。（欄位在 `App`，由 composition 層 `app_ports_p0` 實作這個窄能力。）
pub trait StallTimers: Send + Sync {
    fn stall_timers(&self) -> &tokio::sync::Mutex<std::collections::HashMap<String, u64>>;
}

/// run 最近一次推進度的時間。（欄位在 `App`，由 composition 層 `app_ports_p0` 實作這個窄能力。）
pub trait ProgressEmitted: Send + Sync {
    fn progress_emitted(&self) -> &tokio::sync::Mutex<std::collections::HashMap<String, std::time::Instant>>;
}

/// Fallback timers for hookless completion capture.
pub trait FallbackTimers: Send + Sync {
    fn fallback_timers(&self) -> &tokio::sync::Mutex<std::collections::HashMap<String, u64>>;
}

#[cfg(all(test, feature = "daemon-test-harness"))]
#[path = "poller_tests/reply_window_tests.rs"]
mod reply_window_tests;
