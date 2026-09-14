//! Delivering a prompt: the turn row, the pane write, and the queued path.

use super::*;

/// Close a prompt whose local setup failed after its turn was committed; `pending` must never
/// be the last state the frontend sees.
async fn fail_prompt_delivery(app: &Arc<App>, conversation_id: &str, turn_id: &str, reason: &str) {
    let updated = match sqlx::query(
        "UPDATE turns SET delivery='failed', status='failed', completed_at=? WHERE id=? AND status='in_flight'",
    )
    .bind(db::now())
    .bind(turn_id)
    .execute(&app.db)
    .await
    {
        Ok(result) => result,
        Err(e) => {
            tracing::error!(turn = %turn_id, error = %e, "could not fail prompt delivery");
            return;
        }
    };
    if updated.rows_affected() == 0 {
        return;
    }
    let _ = insert_message(
        app,
        conversation_id,
        Some(turn_id),
        "system",
        &format!("delivery failed: {reason}"),
        "system",
        false,
        None,
    )
    .await;
    emit_turn(app, turn_id).await;
}

async fn emit_prompt_message(app: &Arc<App>, bot_id: &str, message_id: &str) {
    if let Ok(Some(m)) = sqlx::query_as::<_, db::Message>("SELECT * FROM messages WHERE id=?")
        .bind(message_id)
        .fetch_optional(&app.db)
        .await
    {
        app.emit("message_added", json!({"bot_id": bot_id, "message": m})).await;
    }
}


/// What the composer holds right now. `NonEmpty` and `Unready` are deliberately separate: one is
/// someone else's text (a draft the user is typing — never touch it), the other is "the screen
/// does not say" (sol review 2026-09-14 #2 — the old single `Other` sent `ctrl+c` at both).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BoxState {
    /// The bare marker row: nothing typed (a Tip/spinner line elsewhere does not count).
    Empty,
    /// Our text is in there (matched on its head, its tail, or a fragment of it, so wrapping and a
    /// long multi-line paste whose first line scrolled out of the box still count).
    Holds,
    /// Text that is not ours: a draft someone typed in the terminal.
    NonEmpty,
    /// A piece of our text, but not all of it — the box is scrolled, the paste is half in, or the
    /// TUI truncated it. Provable neither way, so it is never typed over and never submitted
    /// (sol review round three #1).
    Truncated,
    /// No readable composer on this screen.
    Unready,
}

/// Display columns: East Asian wide characters take two, box drawing and the rest take one.
/// `screen.rs`'s cruder estimate (everything above U+1100 is wide) would call a row of `─` rules
/// twice its real width, and the pane width is measured from exactly those rules.
pub(crate) fn display_cols(s: &str) -> usize {
    s.chars()
        .map(|c| {
            let u = c as u32;
            let wide = (0x1100..=0x115F).contains(&u)
                || (0x2E80..=0x303E).contains(&u)
                || (0x3041..=0x33FF).contains(&u)
                || (0x3400..=0x4DBF).contains(&u)
                || (0x4E00..=0x9FFF).contains(&u)
                || (0xA000..=0xA4CF).contains(&u)
                || (0xAC00..=0xD7A3).contains(&u)
                || (0xF900..=0xFAFF).contains(&u)
                || (0xFE30..=0xFE6F).contains(&u)
                || (0xFF00..=0xFF60).contains(&u)
                || (0xFFE0..=0xFFE6).contains(&u)
                || (0x20000..=0x3FFFD).contains(&u);
            if wide {
                2
            } else {
                1
            }
        })
        .sum()
}

/// Below this a row cannot have been wrapped by any real terminal, so a following row must be a
/// newline the user typed. Real panes wrap past 100 columns; 60 keeps a margin.
const WRAP_MIN_COLS: usize = 60;

/// The two columns the TUI indents continuation rows by (both soft wraps and typed newlines).
const GUTTER: &str = "  ";

/// Put the TUI's rows back together into the logical lines the user typed.
///
/// Only whitespace that is **provably** the terminal's own is removed: the two-column gutter in
/// front of every continuation row, and the row break after a row that ran to the edge of the
/// pane (a soft wrap). Everything else — the prompt's own newlines and its indentation — is kept,
/// so a code block whose indentation the TUI mangled no longer compares equal (sol review round
/// four). `None` = the rows cannot be reconstructed with certainty; the caller must treat that as
/// unknown, never as a match.
///
/// `wrap_cols` is the pane width when the screen showed it (its full-width rules), else `None`:
/// then any row long enough to have wrapped makes the reconstruction uncertain.
pub(crate) fn rejoin_rows(rows: &[String], wrap_cols: Option<usize>) -> Option<Vec<String>> {
    let threshold = wrap_cols.unwrap_or(WRAP_MIN_COLS);
    let unknown_width = wrap_cols.is_none();
    let mut out: Vec<String> = Vec::new();
    for (i, row) in rows.iter().enumerate() {
        let body = if i == 0 { row.clone() } else { row.strip_prefix(GUTTER).unwrap_or(row).to_string() };
        let body = body.trim_end().to_string();
        let prev_full = out.last().map(|p: &String| display_cols(p) + 1 >= threshold).unwrap_or(false);
        if prev_full {
            if unknown_width {
                // A full-looking row with no known pane width: soft wrap and typed newline are
                // indistinguishable, so refuse to guess.
                return None;
            }
            if let Some(prev) = out.last_mut() {
                prev.push_str(&body);
                continue;
            }
        }
        out.push(body);
    }
    Some(out)
}

/// The pane's width, if the screen drew one of its full-width rules.
pub(crate) fn pane_width(screen: &str) -> Option<usize> {
    screen
        .lines()
        .filter(|l| {
            let t = l.trim();
            t.chars().count() >= 20 && t.chars().all(|c| "─━".contains(c))
        })
        .map(display_cols)
        .max()
}

/// Do these TUI rows say exactly what we sent? Line for line, indentation included.
pub(crate) fn rows_are_text(rows: &[String], wrap_cols: Option<usize>, text: &str) -> bool {
    let Some(lines) = rejoin_rows(rows, wrap_cols) else { return false };
    let want: Vec<&str> = text.lines().map(str::trim_end).collect();
    lines.len() == want.len() && lines.iter().zip(&want).all(|(a, b)| a.trim_end() == *b)
}

/// How many times **the agent has echoed this prompt back** above the composer.
///
/// Only rows the TUI draws for a submitted user message count (claude / grok write the prompt
/// marker at the start of the row). Spinner rows, the status line, the token counter and the
/// composer itself are all excluded, so a redraw can never look like a delivery. The block has to
/// say exactly what we sent — a truncated (`…`) or re-indented echo does not count, and the
/// delivery stays Unknown.
pub(crate) fn echo_hits(kind: &str, screen: &str, text: &str) -> usize {
    if text.trim().is_empty() {
        return 0;
    }
    let width = pane_width(screen);
    echo_blocks(kind, screen).into_iter().filter(|rows| rows_are_text(rows, width, text)).count()
}

/// Every echoed user message above the composer, as the **rows the TUI drew** for it: the marker
/// row (marker stripped) plus its continuation rows, so a multi-line prompt is one block.
pub(crate) fn echo_blocks(kind: &str, screen: &str) -> Vec<Vec<String>> {
    let Some(marker) = prompt_echo_prefix(kind).map(str::trim_end) else { return Vec::new() };
    let lines: Vec<&str> = screen.lines().collect();
    // Everything from the composer's marker row down belongs to the box and the chrome under it.
    let cut = lines.len().saturating_sub(COMPOSER_TAIL);
    let box_row = lines[cut..].iter().rposition(|l| composer_marker_row(l, marker)).map(|i| cut + i);
    let above = &lines[..box_row.unwrap_or(lines.len())];

    let mut out: Vec<Vec<String>> = Vec::new();
    let mut current: Option<Vec<String>> = None;
    for line in above {
        let trimmed = line.trim();
        let is_rule = !trimmed.is_empty() && trimmed.chars().all(|c| "─━-=_╭╮╰╯│".contains(c));
        if let Some(rest) = trimmed.strip_prefix(marker) {
            if let Some(done) = current.take() {
                out.push(done);
            }
            current = Some(vec![rest.trim_start().to_string()]);
            continue;
        }
        // A continuation row is indented under the marker and is plain text; anything else — a
        // blank line, a rule, the agent's own `⏺` reply — ends the block.
        let continued = line.starts_with(GUTTER) && !trimmed.is_empty() && !is_rule && !trimmed.starts_with('⏺') && !trimmed.starts_with('✻');
        match (&mut current, continued) {
            // Keep the row as drawn: `rejoin_rows` is the only place allowed to remove whitespace.
            (Some(block), true) => block.push(line.trim_end().to_string()),
            (slot @ Some(_), false) => {
                if let Some(done) = slot.take() {
                    out.push(done);
                }
            }
            _ => {}
        }
    }
    if let Some(done) = current {
        out.push(done);
    }
    out.retain(|b| b.iter().any(|r| !r.trim().is_empty()));
    out
}

/// Is this row the composer's own marker row (with or without text after it)?
fn composer_marker_row(line: &str, marker: &str) -> bool {
    let t = line.trim().trim_start_matches('│').trim();
    t.starts_with(marker)
}

/// The composer's rows **as drawn** (marker stripped from the first, continuation rows raw), or
/// `None` when the box is empty or there is no readable box. Raw on purpose: only
/// [`rejoin_rows`] may take whitespace away, so the prompt's own indentation survives the trip.
pub(crate) fn composer_rows(kind: &str, screen: &str) -> Option<Vec<String>> {
    let marker = prompt_echo_prefix(kind)?.trim_end();
    if pane_awaits_input(kind, screen) {
        return None;
    }
    let lines: Vec<&str> = screen.lines().collect();
    let from = lines.len().saturating_sub(COMPOSER_TAIL);
    let tail = &lines[from..];
    let idx = tail.iter().rposition(|l| {
        let t = l.trim().trim_start_matches('│').trim();
        t.strip_prefix(marker).map(|rest| !rest.trim().is_empty()).unwrap_or(false)
    })?;
    let mut rows: Vec<String> = Vec::new();
    for (n, line) in tail[idx..].iter().enumerate() {
        let stripped = line.trim_end().trim_start_matches('│').trim_end_matches('│').trim_end();
        if n == 0 {
            let head = stripped.trim_start();
            let rest = head.strip_prefix(marker).unwrap_or(head);
            rows.push(rest.trim_start_matches(' ').to_string());
            continue;
        }
        let t = stripped.trim();
        let is_rule = !t.is_empty() && t.chars().all(|c| "─━-=_╭╮╰╯│".contains(c));
        if t.is_empty() || is_rule {
            break;
        }
        rows.push(stripped.to_string());
    }
    if rows.iter().all(|r| r.trim().is_empty()) {
        return None;
    }
    Some(rows)
}

/// Pure: what is in the composer on this screen.
pub(crate) fn box_state(kind: &str, screen: &str, text: &str) -> BoxState {
    let Some(rows) = composer_rows(kind, screen) else {
        return if pane_awaits_input(kind, screen) { BoxState::Empty } else { BoxState::Unready };
    };
    let width = pane_width(screen);
    // Ours only when the box says exactly what we sent, line for line, indentation included.
    if rows_are_text(&rows, width, text) {
        return BoxState::Holds;
    }
    let Some(lines) = rejoin_rows(&rows, width) else {
        // Cannot undo the TUI's wrapping with certainty: never type over it, never Enter on it.
        return BoxState::Truncated;
    };
    let seen = lines.join("\n");
    let want: String = text.lines().map(str::trim_end).collect::<Vec<_>>().join("\n");
    if !seen.is_empty() && (want.contains(&seen) || seen.contains(&want)) {
        BoxState::Truncated
    } else {
        BoxState::NonEmpty
    }
}

/// The outcome of one delivery attempt. Anything short of `Submitted` is **not** success:
/// sol review 2026-09-14 #1——「證明不了遺失」不等於「已經送出」。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Delivered {
    /// Seen: our text was in the box, and after Enter the box is empty and the screen gained it.
    Submitted,
    /// Could not be proven either way. Caller must park the turn, never re-type on this.
    Unknown(&'static str),
}

/// Pause after typing before the box is read (the TUI has to draw the paste).
const TYPE_SETTLE_MS: u64 = 700;
/// Pause after Enter before checking the prompt left the box.
const SUBMIT_SETTLE_MS: u64 = 1500;
/// Scrollback read for the before/after comparison. Unwrapped so a wrapped line is one row.
const DELIVER_SCAN_LINES: u32 = 400;

/// Decide the outcome from the two screens around Enter. Pure, so every case is a test:
/// `before_enter` is the screen with our text in the box, `after` the one after Enter.
pub(crate) fn submit_outcome(kind: &str, echoes_before: usize, after: &str, text: &str) -> Delivered {
    match box_state(kind, after, text) {
        BoxState::Holds => Delivered::Unknown("still_in_box"),
        BoxState::Truncated => Delivered::Unknown("composer_partial"),
        BoxState::NonEmpty => Delivered::Unknown("composer_busy"),
        BoxState::Unready => Delivered::Unknown("composer_unreadable"),
        // The box held our text and is now empty. That alone is not proof — the TUI can throw the
        // text away — so the agent must also have echoed it back one more time than before.
        BoxState::Empty if echo_hits(kind, after, text) > echoes_before => Delivered::Submitted,
        BoxState::Empty => Delivered::Unknown("no_new_echo"),
    }
}

/// `agent.prompt` on an agent herdr has no session bound to answered ok twice while the text never
/// reached the pane (2026-09-14 wits-c1-op-xh). Treat that as "cannot deliver through the agent".
async fn agent_prompt_usable(client: &HerdrClient, target: &str) -> bool {
    match client.agent_get(target).await {
        Ok(Some(info)) => info.agent_session.is_some(),
        _ => false,
    }
}

/// Deliver a prompt to the agent, and know whether it landed.
///
/// `agent.prompt` (herdr types with bracketed paste and refuses at a dialog with `agent_blocked`)
/// is still the normal path — but only for a run whose pane the daemon has **not** typed into and
/// whose agent herdr has a session bound to. Everything else types into the pane and watches:
///
/// 1. the box must already be **empty**. Someone else's draft is never cleared and never typed
///    over (`composer_busy`); our own leftover text is submitted rather than typed again.
/// 2. after `pane.send_text` our text must be in the box. Only when the box is provably empty and
///    the agent has echoed nothing new is it typed a second time; nothing else is ever re-typed.
/// 3. after Enter the box must be empty **and** the agent must have echoed the prompt once more
///    than before ([`echo_hits`], which ignores spinner and status rows).
///
/// Every read failure is an error, never an empty screen. Everything unproven is `Unknown`, which
/// the caller parks: it must never look like a delivery. Fail closed — when the daemon cannot tell
/// whether this pane needs typing, it types (or gives up), it does not fall back to `agent.prompt`.
pub(crate) async fn deliver_prompt(
    app: &Arc<App>,
    client: &HerdrClient,
    run: &db::Run,
    bot: &db::Bot,
    text: &str,
    force_pane: bool,
) -> anyhow::Result<Delivered> {
    let pane = run.pane_id.as_deref().map(str::trim).filter(|p| !p.is_empty()).map(str::to_string);
    let target = db::run_target(run, bot);
    // Unreadable marker → assume the pane needs typing (sol review #3: a read error used to read
    // as "no, use agent.prompt", which is the path known to swallow prompts).
    let marked = crate::lifecycle::pane_typed_memo(&run.id)
        || match db::pane_typed(&app.db, &run.id).await {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!(run = %run.id, error = %e, "could not read runs.pane_typed; assuming this pane needs typing");
                true
            }
        };
    let must_type = force_pane || marked || !agent_prompt_usable(client, &target).await;
    let Some(pane) = pane.filter(|_| must_type) else {
        if must_type {
            // No pane to type into and the agent cannot be trusted to deliver: say so.
            return Ok(Delivered::Unknown("no_pane_to_type_into"));
        }
        client
            .call_timeout("agent.prompt", json!({"target": target, "text": text}), Duration::from_secs(10))
            .await?;
        return Ok(Delivered::Submitted);
    };
    // Persist "this pane gets typed into" **before** the first keystroke. If the marker cannot be
    // stored, the next prompt (and every prompt after a restart) would go back to the path that
    // silently swallows them — so the delivery is abandoned instead (sol review round three #2).
    // The in-process memo covers the rest of this boot even if the row is later unreadable.
    crate::lifecycle::remember_pane_typed(&run.id);
    if let Err(e) = db::set_pane_typed(&app.db, &run.id).await {
        anyhow::bail!("could not record runs.pane_typed for {} before typing into its pane: {e}", run.id);
    }
    let read = || async { client.pane_read(&pane, "recent-unwrapped", DELIVER_SCAN_LINES).await.map(|r| r.text) };

    // 1. The box has to be ours to use.
    let before = read().await?;
    let echoes_before = echo_hits(&bot.kind, &before, text);
    let mut typed = match box_state(&bot.kind, &before, text) {
        BoxState::Empty => {
            // 2. Type, and see it in the box.
            client.pane_send_text(&pane, text).await?;
            tokio::time::sleep(Duration::from_millis(TYPE_SETTLE_MS)).await;
            let seen = read().await?;
            if box_state(&bot.kind, &seen, text) == BoxState::Empty
                && echo_hits(&bot.kind, &seen, text) == echoes_before
            {
                // Nothing landed anywhere: the box is provably empty, so typing again cannot
                // duplicate anything.
                tracing::warn!(run = %run.id, bot = %bot.name, "typed prompt did not reach the composer; typing it once more");
                client.pane_send_text(&pane, text).await?;
                tokio::time::sleep(Duration::from_millis(TYPE_SETTLE_MS)).await;
                read().await?
            } else {
                seen
            }
        }
        // Our own text from an earlier attempt: submit that, never type it twice.
        BoxState::Holds => {
            tracing::info!(run = %run.id, bot = %bot.name, "the prompt is already in the composer; submitting it instead of typing again");
            before
        }
        // A draft someone is typing in the terminal. Not ours to clear, not ours to type over.
        BoxState::NonEmpty => return Ok(Delivered::Unknown("composer_busy")),
        // Part of our text, or part of something that quotes it: cannot tell, so hands off.
        BoxState::Truncated => return Ok(Delivered::Unknown("composer_partial")),
        BoxState::Unready => return Ok(Delivered::Unknown("composer_unreadable")),
    };
    match box_state(&bot.kind, &typed, text) {
        BoxState::Holds => {}
        BoxState::Empty if echo_hits(&bot.kind, &typed, text) > echoes_before => {
            // The TUI submitted it as it was pasted (bracketed paste with a trailing newline).
            tracing::info!(run = %run.id, bot = %bot.name, "the pasted prompt was submitted without an Enter");
            return Ok(Delivered::Submitted);
        }
        BoxState::Empty => return Ok(Delivered::Unknown("nothing_typed")),
        BoxState::NonEmpty => return Ok(Delivered::Unknown("composer_busy")),
        // Only some of the paste is in the box (or the TUI truncated the row). Never Enter on
        // half a prompt: park it and let the watchdog look again.
        BoxState::Truncated => return Ok(Delivered::Unknown("composer_partial")),
        BoxState::Unready => return Ok(Delivered::Unknown("composer_unreadable")),
    }

    // 3. Enter, and see the agent echo it back.
    client.pane_send_keys(&pane, &["Enter"]).await?;
    tokio::time::sleep(Duration::from_millis(SUBMIT_SETTLE_MS)).await;
    typed = read().await?;
    let outcome = match submit_outcome(&bot.kind, echoes_before, &typed, text) {
        Delivered::Unknown("still_in_box") => {
            // One more Enter, and it is checked too (the second Enter used to be blind).
            tracing::warn!(run = %run.id, bot = %bot.name, "prompt still in the composer after Enter; pressing Enter again");
            client.pane_send_keys(&pane, &["Enter"]).await?;
            tokio::time::sleep(Duration::from_millis(SUBMIT_SETTLE_MS)).await;
            submit_outcome(&bot.kind, echoes_before, &read().await?, text)
        }
        other => other,
    };
    match outcome {
        Delivered::Submitted => tracing::info!(run = %run.id, bot = %bot.name, "prompt typed into the pane and seen echoed back"),
        Delivered::Unknown(why) => tracing::warn!(run = %run.id, bot = %bot.name, reason = why, "could not confirm the prompt was submitted"),
    }
    Ok(outcome)
}

#[derive(serde::Serialize)]
pub struct PromptOut {
    pub turn_id: String,
    pub message_id: String,
    pub delivery: String,
}

pub async fn prompt(app: &Arc<App>, bot_id: &str, text: &str, client_request_id: &str) -> LcResult<PromptOut> {
    prompt_grouped(app, bot_id, text, client_request_id, None, None, &[], None).await
}

/// `prompt` with images (`attach.rs`): the agent gets paths on its host; the timeline renders
/// thumbnails from `messages.attachments_json`.
pub async fn prompt_with(
    app: &Arc<App>,
    bot_id: &str,
    text: &str,
    client_request_id: &str,
    attachment_ids: &[String],
) -> LcResult<PromptOut> {
    prompt_grouped(app, bot_id, text, client_request_id, None, None, attachment_ids, None).await
}

/// 同 `prompt_with`，記下 `relay_from`（bot id 或哨符 `daemon`）；UI 靠它把泡泡畫在左邊。
pub async fn prompt_relayed(
    app: &Arc<App>,
    bot_id: &str,
    text: &str,
    client_request_id: &str,
    attachment_ids: &[String],
    relay_from: Option<&str>,
) -> LcResult<PromptOut> {
    prompt_grouped(app, bot_id, text, client_request_id, None, None, attachment_ids, relay_from).await
}

/// The screen checks every prompt passes before text enters the pane — shared with the queue
/// flush (review 2026-09-12 #6: the flush skipped them and typed into codex's `/model` menu).
/// Refusals insert a system hint and 409 with `needs_login` / `dialog_open` / `picker_open`.
pub(crate) async fn pane_ready_for_prompt(app: &Arc<App>, bot: &db::Bot, run: &db::Run, conv: &str) -> LcResult<()> {
    // An unlogged claude opens on "Select login method" and looks idle; a prompt would type into the menu.
    if bot.kind == "claude" && crate::tui_prompts::stuck_at_login(app, run).await {
        let identity = bot.identity.clone().unwrap_or_default();
        let hint = if identity.is_empty() {
            "這個 claude 還沒登入：到「終端」分頁選 1 完成登入，或在額度那格按「登入」。".to_string()
        } else {
            format!("身份 `{identity}` 還沒登入：到「終端」分頁選 1 完成登入，或在額度那格按「登入」。")
        };
        let _ = insert_message(app, conv, None, "system", &hint, "system", false, None).await;
        return Err(LcError::conflict("needs_login", json!({"run_id": run.id, "identity": identity, "message": hint})));
    }
    // claude「Switch model?」框被 herdr 判成 idle，prompt 打進去會被吃、Enter 按了 Yes（2026-09-11
    // AGM 實測）。還看得到框＝有人在終端手動 `/model`；使用者要送訊息，按 Esc 退掉再送，退不掉就講清楚。
    if bot.kind == "claude" {
        if let Some(pane) = run.pane_id.as_deref().map(str::trim).filter(|p| !p.is_empty()) {
            if let Ok(client) = client_for_run(app, run).await {
                if let Ok(r) = client.pane_read(pane, "visible", 60).await {
                    if crate::tui_prompts::is_switch_model_dialog(&r.text) {
                        let _ = client.pane_send_keys(pane, &["Escape"]).await;
                        tokio::time::sleep(Duration::from_millis(700)).await;
                        let still = matches!(client.pane_read(pane, "visible", 60).await,
                            Ok(r2) if crate::tui_prompts::is_switch_model_dialog(&r2.text));
                        if still {
                            let hint = "claude 的「Switch model?」確認框擋在輸入列前面，關不掉。請到「終端」分頁選 1 或 2 再送一次。";
                            let _ = insert_message(app, conv, None, "system", hint, "system", false, None).await;
                            return Err(LcError::conflict("dialog_open", json!({"run_id": run.id, "message": hint})));
                        }
                        tracing::info!(run = %run.id, "closed a leftover claude model-switch confirmation before delivering a prompt");
                    }
                }
            }
        }
    }
    // codex `/model` 選單開著時，prompt 會變成選單操作、Enter 換掉模型（2026-09-10 實測）。先關掉，關不掉就講清楚。
    if bot.kind == "codex" {
        if let Some(pane) = run.pane_id.as_deref().map(str::trim).filter(|p| !p.is_empty()) {
            if let Ok(client) = client_for_run(app, run).await {
                if !crate::codex_live::close_picker(&client, pane).await {
                    let hint = "codex 的 /model 選單擋在輸入列前面，關不掉。請到「終端」分頁按 Esc 回到輸入列再送一次。";
                    let _ = insert_message(app, conv, None, "system", hint, "system", false, None).await;
                    return Err(LcError::conflict("picker_open", json!({"run_id": run.id, "message": hint})));
                }
            }
        }
    }
    Ok(())
}

pub async fn prompt_grouped(
    app: &Arc<App>,
    bot_id: &str,
    text: &str,
    client_request_id: &str,
    group_id: Option<&str>,
    deliver: Option<&str>,
    attachment_ids: &[String],
    // `None` = 使用者自己在畫面上打的。
    relay_from: Option<&str>,
) -> LcResult<PromptOut> {
    let deliver = deliver.unwrap_or(text);
    let lock = app.bot_lock(bot_id).await;
    let _g = lock.lock().await;

    if client_request_id.trim().is_empty() {
        return Err(LcError::Bad("client_request_id must not be empty".into()));
    }
    let bot = db::bot(&app.db, bot_id).await.map_err(up)?.ok_or_else(|| LcError::NotFound("bot".into()))?;
    let conv = db::conversation_id(&app.db, bot_id).await.map_err(up)?;
    // Resolve first so an unknown id is a plain 400, not an undelivered turn.
    let files = crate::attach::resolve(app, bot_id, attachment_ids)
        .await
        .map_err(|e| LcError::Bad(e.to_string()))?;
    let deliver = crate::attach::deliver_text(deliver, &files);

    // 2. idempotency
    if let Some(t) = sqlx::query_as::<_, db::Turn>("SELECT * FROM turns WHERE conversation_id=? AND client_request_id=?")
        .bind(&conv)
        .bind(client_request_id)
        .fetch_optional(&app.db)
        .await
        .map_err(up)?
    {
        let mid = sqlx::query_scalar::<_, String>("SELECT id FROM messages WHERE turn_id=? AND role='user' LIMIT 1")
            .bind(&t.id)
            .fetch_optional(&app.db)
            .await
            .map_err(up)?
            .unwrap_or_default();
        return Ok(PromptOut { turn_id: t.id, message_id: mid, delivery: t.delivery });
    }

    // 1. preconditions
    let run = db::active_run(&app.db, bot_id).await.map_err(up)?.ok_or_else(|| {
        LcError::conflict("bot has no active run", json!({}))
    })?;
    if run.state != "running" {
        return Err(LcError::conflict("run is not running", json!({"run_id": run.id, "state": run.state})));
    }
    if run.agent_status == "blocked" {
        return Err(LcError::conflict("agent is blocked; answer the prompt first", json!({"run_id": run.id})));
    }
    if let Some(t) = db::in_flight_turn(&app.db, &run.id).await.map_err(up)? {
        return Err(LcError::conflict("a turn is already in flight", json!({"turn_id": t.id})));
    }
    pane_ready_for_prompt(app, &bot, &run, &conv).await?;
    if let Some(t) = sqlx::query_as::<_, db::Turn>(
        "SELECT * FROM turns WHERE conversation_id=? AND delivery='unknown' AND status='in_flight' LIMIT 1",
    )
    .bind(&conv)
    .fetch_optional(&app.db)
    .await
    .map_err(up)?
    {
        return Err(LcError::conflict("a previous turn has unknown delivery; abandon it first", json!({"turn_id": t.id})));
    }
    // Resolve the client before committing: must stay a retryable 502, not a stuck `pending` turn.
    let client = client_for_run(app, &run).await?;

    // 3. turn + user message committed BEFORE the RPC, so an early hook can match.
    let turn_id = db::ulid();
    let mut tx = app.db.begin().await.map_err(up)?;
    sqlx::query(
        "INSERT INTO turns (id, conversation_id, run_id, origin, status, delivery, client_request_id, created_at)
         VALUES (?,?,?,'web','in_flight','pending',?,?)",
    )
    .bind(&turn_id)
    .bind(&conv)
    .bind(&run.id)
    .bind(client_request_id)
    .bind(db::now())
    .execute(&mut *tx)
    .await
    .map_err(up)?;
    let msg_id = db::ulid();
    sqlx::query(
        "INSERT INTO messages (id, conversation_id, turn_id, role, content, source, group_id, relay_from, created_at) VALUES (?,?,?,'user',?,'web',?,?,?)",
    )
    .bind(&msg_id)
    .bind(&conv)
    .bind(&turn_id)
    .bind(text)
    .bind(group_id)
    .bind(relay_from)
    .bind(db::now())
    .execute(&mut *tx)
    .await
    .map_err(up)?;
    tx.commit().await.map_err(up)?;
    if let Err(e) = crate::attach::bind(app, &msg_id, &files).await {
        emit_prompt_message(app, bot_id, &msg_id).await;
        fail_prompt_delivery(app, &conv, &turn_id, &format!("attachment binding failed: {e}")).await;
        return Ok(PromptOut { turn_id, message_id: msg_id, delivery: "failed".into() });
    }
    emit_prompt_message(app, bot_id, &msg_id).await;
    emit_turn(app, &turn_id).await;

    // 4. deliver
    let res = deliver_prompt(app, &client, &run, &bot, &deliver, false).await;
    let delivery = match res {
        Ok(Delivered::Submitted) => "ok",
        // Unproven is not delivered: park the turn (§6.3) instead of arming a watchdog for a
        // prompt that may never have reached the agent.
        Ok(Delivered::Unknown(why)) => {
            tracing::warn!(bot = %bot_id, reason = why, "prompt delivery could not be confirmed");
            "unknown"
        }
        Err(e) => {
            let blocked = e.downcast_ref::<HerdrError>().map(|h| h.code == "agent_blocked").unwrap_or(false);
            if blocked {
                let _ = sqlx::query("UPDATE turns SET delivery='failed', status='failed', completed_at=? WHERE id=?")
                    .bind(db::now())
                    .bind(&turn_id)
                    .execute(&app.db)
                    .await;
                let _ = insert_message(app, &conv, Some(&turn_id), "system", &format!("delivery failed: {e}"), "system", false, None).await;
                emit_turn(app, &turn_id).await;
                return Ok(PromptOut { turn_id, message_id: msg_id, delivery: "failed".into() });
            }
            tracing::warn!(error = %e, "agent.prompt delivery unknown");
            "unknown"
        }
    };
    let _ = sqlx::query("UPDATE turns SET delivery=? WHERE id=?").bind(delivery).bind(&turn_id).execute(&app.db).await;
    emit_turn(app, &turn_id).await;
    if delivery == "ok" {
        arm_stall(app, &run.id, bot_id, &turn_id).await;
        arm_progress(app, &run.id, bot_id, &turn_id).await;
    }
    Ok(PromptOut { turn_id, message_id: msg_id, delivery: delivery.into() })
}

#[cfg(test)]
mod prompt_tests {
    use super::*;
    use crate::testing as tt;

    struct Fixture {
        env: tt::Env,
        bot_id: String,
        conv: String,
        run_id: String,
    }

    async fn fixture(kind: &str, session: &str) -> Fixture {
        let env = tt::env().await;
        let app = env.app.clone();
        let bot_id = db::ulid();
        sqlx::query(
            "INSERT INTO bots (id, project_id, name, kind, args_json, autostart, inject_hooks, hook_token, created_at)
             VALUES (?,?,'prompt-test',?,'[]',0,1,'tok',?)",
        )
        .bind(&bot_id)
        .bind(&env.project_id)
        .bind(kind)
        .bind(db::now())
        .execute(&app.db)
        .await
        .unwrap();
        let conv = db::conversation_id(&app.db, &bot_id).await.unwrap();
        let run_id = db::ulid();
        sqlx::query(
            "INSERT INTO runs (id, bot_id, state, agent_status, herdr_session, started_at)
             VALUES (?,?,'running','idle',?,?)",
        )
        .bind(&run_id)
        .bind(&bot_id)
        .bind(session)
        .bind(db::now())
        .execute(&app.db)
        .await
        .unwrap();
        Fixture { env, bot_id, conv, run_id }
    }

    async fn attachment(app: &Arc<App>, bot_id: &str) -> String {
        let id = db::ulid();
        sqlx::query(
            "INSERT INTO attachments (id, bot_id, name, mime, size, local_path, agent_path, host, created_at)
             VALUES (?,?,'image.png','image/png',1,'/tmp/image.png','/tmp/image.png','local',?)",
        )
        .bind(&id)
        .bind(bot_id)
        .bind(db::now())
        .execute(&app.db)
        .await
        .unwrap();
        id
    }

    /// A missing run session is rejected before writing, so a retry reports the same upstream problem.
    #[tokio::test]
    async fn an_unavailable_run_session_does_not_create_a_turn() {
        let f = fixture("codex", "no-such-session").await;
        let app = f.env.app.clone();

        assert!(matches!(prompt(&app, &f.bot_id, "first", "prompt-1").await, Err(LcError::Upstream(_))));
        assert!(db::in_flight_turn(&app.db, &f.run_id).await.unwrap().is_none());
        let turns: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM turns WHERE conversation_id=?")
            .bind(&f.conv)
            .fetch_one(&app.db)
            .await
            .unwrap();
        assert_eq!(turns, 0, "the unavailable client was checked before INSERT");

        assert!(matches!(prompt(&app, &f.bot_id, "second", "prompt-2").await, Err(LcError::Upstream(_))));
        assert!(db::in_flight_turn(&app.db, &f.run_id).await.unwrap().is_none());
    }

    /// 別的 bot／排程送進來的 prompt 要留 `relay_from`，UI 才分得出來源（2026-09-12 使用者）。
    #[tokio::test]
    async fn a_relayed_prompt_records_who_sent_it() {
        // 一顆 bot 同時只有一個回合在飛：各用一個 fixture。
        let user = fixture("codex", "test").await;
        let user_app = user.env.app.clone();
        let mine = prompt_with(&user_app, &user.bot_id, "使用者自己打的", "prompt-user", &[]).await.unwrap();

        let f = fixture("codex", "test").await;
        let app = f.env.app.clone();
        let relayed = prompt_relayed(&app, &f.bot_id, "排程派的", "prompt-daemon", &[], Some(crate::agent_relay::DAEMON_SENDER))
            .await
            .unwrap();

        let from = |db: sqlx::SqlitePool, id: &str| {
            let db = db.clone();
            let id = id.to_string();
            async move {
                sqlx::query_scalar::<_, Option<String>>("SELECT relay_from FROM messages WHERE id = ?")
                    .bind(&id)
                    .fetch_one(&db)
                    .await
                    .unwrap()
            }
        };
        assert_eq!(from(user_app.db.clone(), &mine.message_id).await, None, "使用者自己打的不該有來源標");
        assert_eq!(
            from(app.db.clone(), &relayed.message_id).await,
            Some(crate::agent_relay::DAEMON_SENDER.to_string())
        );
    }

    /// Binding can fail after the turn commits; the UI must still get a terminal turn event.
    #[tokio::test]
    async fn an_attachment_bind_failure_closes_the_pending_turn() {
        let success = fixture("codex", "test").await;
        let success_app = success.env.app.clone();
        let success_attachment = attachment(&success_app, &success.bot_id).await;
        let mut success_events = success_app.subscribe();
        let success_out = prompt_with(&success_app, &success.bot_id, "look", "prompt-attachments-ok", &[success_attachment])
            .await
            .unwrap();
        let success_event = tokio::time::timeout(std::time::Duration::from_secs(1), success_events.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(success_event.kind, "message_added");
        assert_eq!(success_event.data["message"]["id"], success_out.message_id);
        assert!(!success_event.data["message"]["attachments_json"].is_null());

        let f = fixture("codex", "test").await;
        let app = f.env.app.clone();
        let attachment_id = attachment(&app, &f.bot_id).await;
        sqlx::query(
            "CREATE TRIGGER fail_prompt_attachment_bind
             BEFORE UPDATE OF message_id ON attachments
             BEGIN SELECT RAISE(ABORT, 'bind failed'); END",
        )
        .execute(&app.db)
        .await
        .unwrap();
        let mut ws_events = app.subscribe();
        let mut turn_events = app.subscribe_turns();

        let out = prompt_with(&app, &f.bot_id, "look", "prompt-attachments", &[attachment_id]).await.unwrap();
        assert_eq!(out.delivery, "failed");
        let user_event = tokio::time::timeout(std::time::Duration::from_secs(1), ws_events.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(user_event.kind, "message_added");
        assert_eq!(user_event.data["message"]["id"], out.message_id);
        assert_eq!(user_event.data["message"]["role"], "user");
        assert!(db::in_flight_turn(&app.db, &f.run_id).await.unwrap().is_none());
        let turn: db::Turn = sqlx::query_as("SELECT * FROM turns WHERE id=?")
            .bind(&out.turn_id)
            .fetch_one(&app.db)
            .await
            .unwrap();
        assert_eq!((turn.status.as_str(), turn.delivery.as_str()), ("failed", "failed"));
        let system: String = sqlx::query_scalar("SELECT content FROM messages WHERE turn_id=? AND role='system'")
            .bind(&out.turn_id)
            .fetch_one(&app.db)
            .await
            .unwrap();
        assert!(system.contains("attachment binding failed"));
        let event = tokio::time::timeout(std::time::Duration::from_secs(1), turn_events.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(event.turn_id, out.turn_id);
        assert_eq!((event.status.as_str(), event.delivery.as_str()), ("failed", "failed"));
    }
}


#[cfg(test)]
mod delivery_tests {
    use super::*;

    /// 真實畫面骨架（2026-09-14 w1HJ:pM 那類 claude）：對話區 + spinner 行 + 輸入框 + 狀態列。
    fn screen(transcript: &str, box_rows: &[&str]) -> String {
        let mut s = String::new();
        s.push_str(transcript);
        s.push_str("\n✻ Sautéed for 15m 9s · done 2:18 PM\n");
        s.push_str("─────────────────────────────────────────────\n");
        if box_rows.is_empty() {
            s.push_str("❯\n");
        } else {
            s.push_str(&format!("❯ {}\n", box_rows[0]));
            for r in &box_rows[1..] {
                s.push_str(&format!("  {r}\n"));
            }
        }
        s.push_str("─────────────────────────────────────────────\n");
        s.push_str("  user. | web | OP5 61% | 5h:53% | 7d:95%\n");
        s.push_str("  ⏵⏵ bypass permissions on (shift+tab to cycle) · ← for agents\n");
        s
    }

    const TEXT: &str = "加一個功能除了按 SKU 之外，也要能用品名批次置換";
    const HISTORY: &str = "❯ 加一個功能除了按 SKU 之外，也要能用品名批次置換\n⏺ 好，我看一下。\n";

    #[test]
    fn an_empty_box_our_text_someone_elses_draft_and_an_unreadable_screen_are_four_things() {
        assert_eq!(box_state("claude", &screen("⏺ 先前的回覆\n", &[]), TEXT), BoxState::Empty);
        assert_eq!(box_state("claude", &screen("⏺ 先前的回覆\n", &[TEXT]), TEXT), BoxState::Holds);
        // 使用者自己在終端打到一半的字：不是我們的，不能清也不能蓋。
        assert_eq!(box_state("claude", &screen("⏺ 先前的回覆\n", &["我自己在打的草稿"]), TEXT), BoxState::NonEmpty);
        // 完全沒有輸入框可讀（畫面被別的東西佔滿）。
        assert_eq!(box_state("claude", "Select login method:\n  1. Claude account\n", TEXT), BoxState::Unready);
    }

    /// 真的排到行尾才是軟折行，接回來要跟整段一字不差；只看得到一截＝判不出來，不能按 Enter。
    #[test]
    fn only_the_whole_prompt_in_the_box_counts_as_ours() {
        // 一行長訊息被終端機折成兩列（第一列排到行尾），還原後跟原文相同。
        let long = "please rewrite the offline quote importer so it is much faster";
        let wrapped = screen("⏺ 先前的回覆\n", &["please rewrite the offline quote importer so", " it is much faster"]);
        assert_eq!(box_state("claude", &wrapped, long), BoxState::Holds);
        // 多行貼上只落了第一行：以前算 Holds 會直接按 Enter 送半段出去。
        let multi = "第一行：先看報告\n第二行：再改程式";
        let first_line_only = screen("⏺ 先前的回覆\n", &["第一行：先看報告"]);
        assert_eq!(box_state("claude", &first_line_only, multi), BoxState::Truncated);
        // 使用者草稿剛好引用了任務片段，也不能被當成我們的字。
        let quoting_draft = screen("⏺ 先前的回覆\n", &["第二行：再改程式"]);
        assert_eq!(box_state("claude", &quoting_draft, multi), BoxState::Truncated);
        // 完全不相干的草稿。
        let draft = screen("⏺ 先前的回覆\n", &["我自己在打的別的東西"]);
        assert_eq!(box_state("claude", &draft, multi), BoxState::NonEmpty);
    }

    /// 多行回音要整塊解析：頭尾都對才算送出，被截斷（…）判不出來就不算。
    #[test]
    fn a_multi_line_echo_is_matched_as_one_block() {
        let multi = "第一行：先看報告\n第二行：再改程式\n第三行：最後回報";
        let after = screen("⏺ 先前的回覆\n❯ 第一行：先看報告\n  第二行：再改程式\n  第三行：最後回報\n", &[]);
        assert_eq!(echo_blocks("claude", &after).len(), 1, "三列是一則，不是三則");
        assert_eq!(echo_hits("claude", &after, multi), 1);
        assert_eq!(submit_outcome("claude", 0, &after, multi), Delivered::Submitted);
        // 只回音了第一行（TUI 截斷或只送出半段）：判不出整段送出。
        let partial = screen("⏺ 先前的回覆\n❯ 第一行：先看報告\n", &[]);
        assert_eq!(echo_hits("claude", &partial, multi), 0);
        assert_eq!(submit_outcome("claude", 0, &partial, multi), Delivered::Unknown("no_new_echo"));
        // 尾巴被省略號吃掉的一列也不算。
        let clipped = screen("⏺ 先前的回覆\n❯ 第一行：先看報告 第二行：再改程式…\n", &[]);
        assert_eq!(echo_hits("claude", &clipped, multi), 0);
    }

    /// 只有「agent 把這句回音在對話區多印了一次」才算送出。
    #[test]
    fn only_a_new_echo_above_the_box_counts_as_submitted() {
        let before = screen("⏺ 先前的回覆\n", &[]);
        let after = screen(&format!("⏺ 先前的回覆\n❯ {TEXT}\n"), &[]);
        assert_eq!(echo_hits("claude", &before, TEXT), 0);
        assert_eq!(echo_hits("claude", &after, TEXT), 1);
        assert_eq!(submit_outcome("claude", 0, &after, TEXT), Delivered::Submitted);
        assert_eq!(submit_outcome("claude", 0, &before, TEXT), Delivered::Unknown("no_new_echo"));
    }

    /// 對話裡早就有同一句：只有再多一次才算這一次送出的。
    #[test]
    fn an_identical_prompt_already_in_the_history_is_not_mistaken_for_this_one() {
        let before = screen(HISTORY, &[]);
        let echoes_before = echo_hits("claude", &before, TEXT);
        assert_eq!(echoes_before, 1, "舊的那一句本來就在畫面上");
        assert_eq!(submit_outcome("claude", echoes_before, &before, TEXT), Delivered::Unknown("no_new_echo"));
        let after = screen(&format!("{HISTORY}❯ {TEXT}\n"), &[]);
        assert_eq!(submit_outcome("claude", echoes_before, &after, TEXT), Delivered::Submitted);
    }

    #[test]
    fn text_left_in_the_box_or_replaced_is_never_counted_as_submitted() {
        let in_box = screen("⏺ 先前的回覆\n", &[TEXT]);
        assert_eq!(submit_outcome("claude", 0, &in_box, TEXT), Delivered::Unknown("still_in_box"));
        let draft = screen("⏺ 先前的回覆\n", &["我自己在打的草稿"]);
        assert_eq!(submit_outcome("claude", 0, &draft, TEXT), Delivered::Unknown("composer_busy"));
        assert_eq!(submit_outcome("claude", 0, "Select login method:\n 1. …\n", TEXT), Delivered::Unknown("composer_unreadable"));
    }

    /// spinner／計時／狀態列刷新都不是證據：畫面變了但沒有新回音 → 不算送出（sol review 二輪 #1）。
    #[test]
    fn a_redrawn_spinner_or_status_line_is_not_evidence_of_delivery() {
        let before = screen("⏺ 先前的回覆\n✻ Crunching… (3s · esc to interrupt)\n", &[]);
        let redrawn = screen("⏺ 先前的回覆\n✻ Crunching… (9s · esc to interrupt)\n", &[])
            .replace("5h:53%", "5h:52%");
        assert_ne!(before, redrawn, "畫面確實不一樣了");
        assert_eq!(submit_outcome("claude", 0, &redrawn, TEXT), Delivered::Unknown("no_new_echo"));
        // 短 prompt 也一樣，不能因為畫面變了就當送出。
        let short = "go";
        assert_eq!(submit_outcome("claude", 0, &redrawn, short), Delivered::Unknown("no_new_echo"));
        // 短 prompt 的證據就是它自己的回音行。
        let echoed = screen("⏺ 先前的回覆\n❯ go\n⏺ 好\n", &[]);
        assert_eq!(echo_hits("claude", &echoed, short), 1);
        assert_eq!(submit_outcome("claude", 0, &echoed, short), Delivered::Submitted);
    }

    /// 縮排與硬換行是內容的一部分：程式碼被 TUI 弄壞縮排就不是「同一段」（sol review 第四輪）。
    #[test]
    fn indentation_and_hard_newlines_are_part_of_the_text() {
        let code = "修這段：\nfn main() {\n    println!(\"hi\");\n}";
        let intact = screen("⏺ 先前的回覆\n", &["修這段：", "fn main() {", "    println!(\"hi\");", "}"]);
        assert_eq!(box_state("claude", &intact, code), BoxState::Holds);
        // 縮排被吃掉：以前 squash 之後照樣相等，現在不算我們的字。
        let flattened = screen("⏺ 先前的回覆\n", &["修這段：", "fn main() {", "println!(\"hi\");", "}"]);
        assert_ne!(box_state("claude", &flattened, code), BoxState::Holds);
        // 換行被併成一行也不算。
        let joined = screen("⏺ 先前的回覆\n", &["修這段： fn main() { println!(\"hi\"); }"]);
        assert_ne!(box_state("claude", &joined, code), BoxState::Holds);
        // 回音同理：縮排壞掉就不算送到。
        let echoed_flat = screen("❯ 修這段：\n  fn main() {\n  println!(\"hi\");\n  }\n", &[]);
        assert_eq!(echo_hits("claude", &echoed_flat, code), 0);
        let echoed_ok = screen("❯ 修這段：\n  fn main() {\n      println!(\"hi\");\n  }\n", &[]);
        assert_eq!(echo_hits("claude", &echoed_ok, code), 1);
    }

    /// 只有排到行尾的那種折行才可以接回去；短行後面的下一列是使用者自己按的換行。
    #[test]
    fn only_a_row_that_ran_to_the_edge_is_treated_as_a_soft_wrap() {
        let width = Some(40);
        let wrapped = vec!["這是一段很長的中文會被終端機折到下一行去".to_string(), "  繼續講完這句".to_string()];
        assert_eq!(
            rejoin_rows(&wrapped, width).unwrap(),
            vec!["這是一段很長的中文會被終端機折到下一行去繼續講完這句".to_string()],
            "排到行尾＝軟折行，接回去",
        );
        let typed = vec!["短短一行".to_string(), "  第二行".to_string()];
        assert_eq!(
            rejoin_rows(&typed, width).unwrap(),
            vec!["短短一行".to_string(), "第二行".to_string()],
            "沒排到行尾＝使用者自己按的換行，保留",
        );
        // 量不到 pane 寬度、又有長到可能折行的列：還原不了就不猜。
        let long = vec!["x".repeat(80), "  continued".to_string()];
        assert!(rejoin_rows(&long, None).is_none());
    }

    /// 回音只認對話區：輸入框裡的同一句、spinner 行裡出現的字都不算。
    #[test]
    fn echoes_are_counted_above_the_box_only() {
        let in_box = screen("⏺ 先前的回覆\n", &[TEXT]);
        assert_eq!(echo_hits("claude", &in_box, TEXT), 0, "還在框裡不是回音");
        let both = screen(&format!("⏺ 先前的回覆\n❯ {TEXT}\n"), &[TEXT]);
        assert_eq!(echo_hits("claude", &both, TEXT), 1, "只算框上面那一次");
    }
}
