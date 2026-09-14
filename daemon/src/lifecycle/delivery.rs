//! Typing a prompt into a pane, and proving it was submitted — without guessing.
//!
//! Why this exists: herdr `agent.prompt` answered ok twice on wits-c1-op-xh (2026-09-14 14:24 and
//! 15:33, the second two minutes after a live `/effort`) while the text never reached the pane.
//! Those runs type into the pane instead, and a delivery only counts when it can be **proven**.
//!
//! The text as the TUI drew it is never used to rebuild the prompt (sol review rounds one to
//! five: soft wraps look like typed newlines, trailing spaces are invisible, character widths are
//! unreliable). The evidence is fixed **before typing**, and only two kinds are accepted:
//!
//! * **the agent's own transcript** (claude, local host, bound to the run's current session):
//!   the bytes appended after the baseline must contain a user entry that is exactly the prompt.
//! * **one echo row** (when there is no transcript): only for a one-line prompt that provably fits
//!   on one row of the pane at its current width, so a wrap cannot happen; the submitted message
//!   must appear as exactly one row `❯ <text>` with no continuation row under it.
//!
//! Outcomes are three, and they mean different things to the caller (sol review round seven #2):
//! `Submitted`; `NotAttempted` — **nothing was sent**, the turn must not be left in flight; and
//! `Unproven` — keys were sent and the result cannot be proven, which is what `unknown` means.

use super::*;
use std::time::Duration;

/// What the composer holds right now. Ownership is never inferred from text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BoxState {
    /// Exactly the bare marker row, with the box's own rule directly under it.
    Empty,
    /// Anything else in the box — a draft, a lone space, a blank second line, a suggestion.
    NonEmpty,
    /// No readable composer on this screen.
    Unready,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Delivered {
    Submitted,
    /// Nothing reached the pane or the agent. `retry` = the condition can clear by itself (a busy
    /// box, a transcript not yet reported); `false` = this prompt can never be proven on this run.
    NotAttempted { reason: &'static str, retry: bool },
    /// Keys were sent; whether the agent took the prompt cannot be proven.
    Unproven(&'static str),
    /// Typed and submitted (the box took the paste and emptied on Enter), on a run where no
    /// lossless evidence exists — grok, remote hosts, a codex session not reported yet. Delivered
    /// as far as the screen can tell, **never** re-sent, and marked for a human to check.
    Unverified,
}

/// Which agent wrote a session log, i.e. how to read a user entry out of it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LogFormat {
    /// claude `projects/<cwd>/<session>.jsonl`: `{"type":"user","message":{"content":…}}`.
    Claude,
    /// codex `sessions/YYYY/MM/DD/rollout-…-<session>.jsonl`:
    /// `{"type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text",…}]}}`.
    Codex,
}

/// The evidence a delivery will be judged by.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Proof {
    /// Count single echo rows equal to the prompt.
    EchoRow,
    /// Count exact user entries appended to the agent's own session log after the baseline offset,
    /// while the run still points at this session.
    Transcript { format: LogFormat, path: std::path::PathBuf, session_id: String },
    /// No lossless evidence on this run: type, submit, and report `Unverified`.
    Unverified,
}

/// How the prompt will be delivered, decided before anything is written.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Plan {
    AgentPrompt { target: String },
    Type { pane: String, proof: Proof },
}

const TYPE_SETTLE_MS: u64 = 700;
const SUBMIT_SETTLE_MS: u64 = 1200;
const SUBMIT_CHECKS: u32 = 3;
const DELIVER_SCAN_LINES: u32 = 400;
/// Longest prompt the daemon will type and try to prove.
pub(crate) const MAX_PROVABLE_CHARS: usize = 200_000;
/// Columns kept free at the right edge when deciding a prompt fits on one row: the TUI's own
/// padding and cursor cell. Generous on purpose — a false "does not fit" only costs a deferral.
const ROW_SAFETY_COLS: usize = 6;

/// The echo markers a kind draws at the start of a message; matched as exact row prefixes.
pub(crate) fn echo_markers(kind: &str) -> &'static [&'static str] {
    match kind {
        "claude" => &["❯ ", "> "],
        "grok" => &["❯ "],
        "codex" => &["› "],
        _ => &[],
    }
}

/// Characters whose rendered form is not a dependable copy of the input.
fn uncertain_char(c: char) -> bool {
    let u = c as u32;
    c.is_control()
        || matches!(u, 0x200B..=0x200F | 0x2028..=0x202E | 0x2060..=0x206F | 0xFEFF)
        || matches!(u, 0xFE00..=0xFE0F | 0xE0100..=0xE01EF)
        || matches!(u, 0x0300..=0x036F | 0x0483..=0x0489 | 0x0591..=0x05BD | 0x0610..=0x061A | 0x064B..=0x065F)
        || matches!(u, 0x1AB0..=0x1AFF | 0x1DC0..=0x1DFF | 0x20D0..=0x20FF | 0xFE20..=0xFE2F)
        || matches!(u, 0x3099..=0x309A)
}

/// An upper bound on the columns `text` can take: ASCII is one column, everything else is
/// counted as two. Over-estimating only makes the fit check stricter.
fn max_cols(text: &str) -> usize {
    text.chars().map(|c| if c.is_ascii() { 1 } else { 2 }).sum()
}

/// Can one echo row prove this prompt on a pane `pane_cols` wide? One line, nothing invisible at
/// either end, nothing of uncertain rendering, and short enough that the marker plus the text plus
/// a safety margin fit on one row — so a wrap is impossible, not merely unlikely.
pub(crate) fn provable_by_echo_row(text: &str, marker_cols: usize, pane_cols: Option<u32>) -> bool {
    let Some(cols) = pane_cols else { return false };
    !text.is_empty()
        && !text.contains('\n')
        && !text.contains('\r')
        && text == text.trim()
        && !text.chars().any(uncertain_char)
        && marker_cols + max_cols(text) + ROW_SAFETY_COLS <= cols as usize
}

fn is_rule_row(row: &str) -> bool {
    let t = row.trim();
    !t.is_empty() && t.chars().all(|c| "─━╭╮╰╯".contains(c))
}

/// Index of the composer's marker row: the last row near the bottom that starts with a marker or
/// is a bare marker.
fn composer_row(kind: &str, lines: &[&str]) -> Option<usize> {
    let markers = echo_markers(kind);
    let from = lines.len().saturating_sub(COMPOSER_TAIL);
    lines[from..]
        .iter()
        .rposition(|l| {
            let t = l.trim_start_matches('│');
            markers.iter().any(|m| t.starts_with(m) || t == m.trim_end())
        })
        .map(|i| from + i)
}

/// Pure: what the composer holds. `Empty` only for the exact known shape — a bare marker row
/// (`❯` or `❯ ` with nothing after it) with the box's rule directly under it. A lone space, a
/// blank second row, a suggestion, a draft: all `NonEmpty` (sol review round seven #1).
pub(crate) fn box_state(kind: &str, screen: &str) -> BoxState {
    let lines: Vec<&str> = screen.lines().collect();
    let Some(idx) = composer_row(kind, &lines) else { return BoxState::Unready };
    let row = lines[idx].trim_start_matches('│').trim_end_matches('│');
    let bare = echo_markers(kind).iter().any(|m| row == *m || row == m.trim_end());
    let closed = lines.get(idx + 1).map(|r| is_rule_row(r)).unwrap_or(false);
    match (bare, closed) {
        (true, true) if pane_awaits_input(kind, screen) => BoxState::Empty,
        (true, true) => BoxState::Unready,
        _ => BoxState::NonEmpty,
    }
}

fn continuation_row(row: &str) -> bool {
    row.strip_prefix("  ").map(|rest| !rest.trim().is_empty()).unwrap_or(false)
        && !row.trim_start().starts_with(['⏺', '✻', '⎿', '●', '─', '│'])
}

/// How many single, un-continued echo rows above the composer say exactly `text`.
pub(crate) fn echo_row_hits(kind: &str, screen: &str, text: &str) -> usize {
    let lines: Vec<&str> = screen.lines().collect();
    let end = composer_row(kind, &lines).unwrap_or(lines.len());
    let markers = echo_markers(kind);
    (0..end)
        .filter(|&i| {
            let row = lines[i];
            let exact = markers.iter().any(|m| row.strip_prefix(m) == Some(text));
            let continued = i + 1 < end && continuation_row(lines[i + 1]);
            exact && !continued
        })
        .count()
}

/// The text of one session-log line if it is a user message a person typed.
pub(crate) fn log_user_text(format: LogFormat, line: &str) -> Option<String> {
    match format {
        LogFormat::Claude => transcript_user_text(line),
        LogFormat::Codex => codex_user_text(line),
    }
}

/// codex rollout: only a top-level `response_item` user message counts. `compacted` entries replay
/// old history inside `replacement_history` and are not new messages; parts other than
/// `input_text` (images, files) make the entry something other than our typed prompt.
pub(crate) fn codex_user_text(line: &str) -> Option<String> {
    let v: Value = serde_json::from_str(line).ok()?;
    if v.get("type").and_then(Value::as_str) != Some("response_item") {
        return None;
    }
    let p = v.get("payload")?;
    if p.get("type").and_then(Value::as_str) != Some("message") || p.get("role").and_then(Value::as_str) != Some("user") {
        return None;
    }
    let parts = p.get("content")?.as_array()?;
    if parts.is_empty() || parts.iter().any(|x| x.get("type").and_then(Value::as_str) != Some("input_text")) {
        return None;
    }
    Some(parts.iter().filter_map(|x| x.get("text").and_then(Value::as_str)).collect::<Vec<_>>().join("\n"))
}

pub(crate) fn transcript_user_text(line: &str) -> Option<String> {
    let v: Value = serde_json::from_str(line).ok()?;
    if v.get("type").and_then(Value::as_str) != Some("user") || v.get("isMeta").and_then(Value::as_bool) == Some(true) {
        return None;
    }
    let content = v.get("message")?.get("content")?;
    match content {
        Value::String(s) => Some(s.clone()),
        Value::Array(parts) => {
            if parts.iter().any(|p| p.get("type").and_then(Value::as_str) != Some("text")) {
                return None;
            }
            Some(parts.iter().filter_map(|p| p.get("text").and_then(Value::as_str)).collect::<Vec<_>>().join("\n"))
        }
        _ => None,
    }
}

pub(crate) fn transcript_len(path: &std::path::Path) -> std::io::Result<u64> {
    Ok(std::fs::metadata(path)?.len())
}

/// Exact user entries for `text` in the bytes of `path` from `offset` on. Only what was appended
/// after the baseline is read, so an old identical message sliding out of any window cannot hide
/// a new one, and the cost is the new bytes only. A partial first line fails to parse and is
/// skipped; a partial last line is simply not complete yet. `Err` = unreadable, never zero.
pub(crate) fn transcript_hits_since(path: &std::path::Path, offset: u64, text: &str) -> std::io::Result<usize> {
    log_hits_since(LogFormat::Claude, path, offset, text)
}

pub(crate) fn log_hits_since(format: LogFormat, path: &std::path::Path, offset: u64, text: &str) -> std::io::Result<usize> {
    use std::io::{Read, Seek, SeekFrom};
    let mut f = std::fs::File::open(path)?;
    let len = f.metadata()?.len();
    if len < offset {
        // The file was replaced or truncated: whatever is there is not the baseline we took.
        return Err(std::io::Error::new(std::io::ErrorKind::InvalidData, "transcript shrank below the baseline"));
    }
    f.seek(SeekFrom::Start(offset))?;
    let mut buf = Vec::new();
    f.read_to_end(&mut buf)?;
    let body = String::from_utf8_lossy(&buf);
    Ok(body.lines().filter_map(|l| log_user_text(format, l)).filter(|t| t == text).count())
}

/// Day directories searched for a codex rollout, newest first. A session that has run longer than
/// this is resumed into a new rollout file anyway.
const CODEX_LOG_DAYS: usize = 14;

/// Find the rollout file codex writes for `session_id` under `codex_home/sessions`. Only the file
/// named for exactly this session counts; `None` when it is not there.
pub(crate) fn codex_session_log(codex_home: &std::path::Path, session_id: &str) -> Option<std::path::PathBuf> {
    let id = session_id.trim();
    if id.is_empty() || !id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-') {
        return None;
    }
    let suffix = format!("-{id}.jsonl");
    let sorted_dirs = |dir: &std::path::Path| -> Vec<std::path::PathBuf> {
        let mut v: Vec<std::path::PathBuf> = std::fs::read_dir(dir)
            .map(|rd| rd.filter_map(|e| e.ok()).map(|e| e.path()).filter(|p| p.is_dir()).collect())
            .unwrap_or_default();
        v.sort();
        v.reverse();
        v
    };
    let mut days = Vec::new();
    for year in sorted_dirs(&codex_home.join("sessions")) {
        for month in sorted_dirs(&year) {
            for day in sorted_dirs(&month) {
                days.push(day);
                if days.len() >= CODEX_LOG_DAYS {
                    break;
                }
            }
            if days.len() >= CODEX_LOG_DAYS {
                break;
            }
        }
        if days.len() >= CODEX_LOG_DAYS {
            break;
        }
    }
    days.into_iter().find_map(|day| {
        std::fs::read_dir(&day).ok()?.filter_map(|e| e.ok()).map(|e| e.path()).find(|p| {
            p.file_name().and_then(|n| n.to_str()).map(|n| n.starts_with("rollout-") && n.ends_with(&suffix)).unwrap_or(false)
        })
    })
}

/// `CODEX_HOME` for this bot on the local host: identity env, then the bot's own env, else `~/.codex`.
pub(crate) async fn codex_home(app: &Arc<App>, bot: &db::Bot) -> Option<std::path::PathBuf> {
    let home = dirs::home_dir()?.to_string_lossy().into_owned();
    let mut value: Option<String> = None;
    if let Some(name) = bot.identity.as_deref().filter(|s| !s.is_empty()) {
        if let Some(id) = crate::tools::identity_for_host(app, LOCAL_HOST, name).await {
            if let Some(v) = id.env.get("CODEX_HOME") {
                value = Some(v.clone());
            }
        }
    }
    if let Some(v) = bot.env().get("CODEX_HOME") {
        value = Some(v.clone());
    }
    let dir = value.map(|v| v.trim().to_string()).filter(|v| !v.is_empty()).map(|v| crate::config::expand_home(&v, &home));
    Some(std::path::PathBuf::from(dir.unwrap_or_else(|| format!("{home}/.codex"))))
}

/// Choose the evidence. Transcript first whenever the run has a current, local claude session;
/// the echo row only when there is no transcript and the prompt provably fits on one row.
pub(crate) struct ProofInputs<'a> {
    pub kind: &'a str,
    pub host_is_local: bool,
    /// Whether the daemon injected hooks: without them claude never reports a transcript.
    pub hooks: bool,
    pub session_id: Option<&'a str>,
    pub transcript_path: Option<&'a str>,
    /// The codex rollout for `session_id`, when found.
    pub codex_log: Option<std::path::PathBuf>,
    pub pane_cols: Option<u32>,
}

/// The evidence matrix (SPEC §4.4a). Lossless evidence when it exists; otherwise the prompt is
/// still typed and reported `Unverified` — never refused, never re-sent.
pub(crate) fn choose_proof(i: &ProofInputs, text: &str) -> Result<Proof, Delivered> {
    if text.chars().count() > MAX_PROVABLE_CHARS {
        return Err(Delivered::NotAttempted { reason: "prompt_too_long_to_prove", retry: false });
    }
    let session = i.session_id.map(str::trim).filter(|s| !s.is_empty());
    let path = i.transcript_path.map(str::trim).filter(|p| !p.is_empty());
    match (i.kind, i.host_is_local) {
        ("claude", true) => {
            if let (Some(session), Some(path)) = (session, path) {
                if std::path::Path::new(path).is_file() {
                    return Ok(Proof::Transcript { format: LogFormat::Claude, path: path.into(), session_id: session.to_string() });
                }
            }
        }
        ("codex", true) => {
            if let (Some(session), Some(log)) = (session, i.codex_log.as_ref()) {
                if log.is_file() {
                    return Ok(Proof::Transcript { format: LogFormat::Codex, path: log.clone(), session_id: session.to_string() });
                }
            }
        }
        _ => {}
    }
    let marker_cols = echo_markers(i.kind).first().map(|m| m.chars().count()).unwrap_or(0);
    if marker_cols > 0 && provable_by_echo_row(text, marker_cols, i.pane_cols) {
        return Ok(Proof::EchoRow);
    }
    // A local claude with hooks reports its transcript at SessionStart: worth a short wait, since a
    // lossless proof is coming. Everything else has no lossless evidence to wait for.
    if i.kind == "claude" && i.host_is_local && i.hooks && (session.is_none() || path.is_none()) {
        return Err(Delivered::NotAttempted { reason: "transcript_not_ready", retry: true });
    }
    Ok(Proof::Unverified)
}

/// `agent.prompt` on an agent herdr has no session bound to answered ok while the text never
/// reached the pane (2026-09-14 wits-c1-op-xh).
async fn agent_prompt_usable(client: &HerdrClient, target: &str) -> bool {
    match client.agent_get(target).await {
        Ok(Some(info)) => info.agent_session.is_some(),
        _ => false,
    }
}

/// Decide how to deliver, touching nothing: the route, the evidence and an empty box. Callers run
/// this **before** committing a turn, so a prompt that cannot be sent never becomes one.
pub(crate) async fn plan_delivery(
    app: &Arc<App>,
    client: &HerdrClient,
    run: &db::Run,
    bot: &db::Bot,
    text: &str,
    force_pane: bool,
) -> anyhow::Result<Result<Plan, Delivered>> {
    let target = db::run_target(run, bot);
    let marked = crate::lifecycle::pane_typed_memo(&run.id)
        || match db::pane_typed(&app.db, &run.id).await {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!(run = %run.id, error = %e, "could not read runs.pane_typed; assuming this pane needs typing");
                true
            }
        };
    if !(force_pane || marked || !agent_prompt_usable(client, &target).await) {
        return Ok(Ok(Plan::AgentPrompt { target }));
    }
    let Some(pane) = run.pane_id.as_deref().map(str::trim).filter(|p| !p.is_empty()).map(str::to_string) else {
        return Ok(Err(Delivered::NotAttempted { reason: "no_pane_to_type_into", retry: true }));
    };
    let host_is_local = matches!(db::project(&app.db, &bot.project_id).await, Ok(Some(p)) if p.host == LOCAL_HOST);
    let pane_cols = client.pane_size(&pane).await.ok().flatten().map(|(w, _)| w);
    let codex_log = match (bot.kind.as_str(), host_is_local, run.native_session_id.as_deref()) {
        ("codex", true, Some(session)) => codex_home(app, bot).await.and_then(|h| codex_session_log(&h, session)),
        _ => None,
    };
    let inputs = ProofInputs {
        kind: &bot.kind,
        host_is_local,
        hooks: bot.inject_hooks != 0,
        session_id: run.native_session_id.as_deref(),
        transcript_path: run.transcript_path.as_deref(),
        codex_log,
        pane_cols,
    };
    let proof = match choose_proof(&inputs, text) {
        Ok(p) => p,
        Err(not) => return Ok(Err(not)),
    };
    let screen = client.pane_read(&pane, "recent-unwrapped", DELIVER_SCAN_LINES).await?.text;
    match box_state(&bot.kind, &screen) {
        BoxState::Empty => Ok(Ok(Plan::Type { pane, proof })),
        BoxState::NonEmpty => Ok(Err(Delivered::NotAttempted { reason: "composer_busy", retry: true })),
        BoxState::Unready => Ok(Err(Delivered::NotAttempted { reason: "composer_unreadable", retry: true })),
    }
}

/// Current evidence count for `proof`.
fn evidence(kind: &str, proof: &Proof, offset: u64, screen: &str, text: &str) -> std::io::Result<usize> {
    match proof {
        Proof::EchoRow => Ok(echo_row_hits(kind, screen, text)),
        Proof::Transcript { format, path, .. } => log_hits_since(*format, path, offset, text),
        Proof::Unverified => Ok(0),
    }
}

/// Is the run still on the session the transcript proof was taken from?
async fn same_session(app: &Arc<App>, run_id: &str, proof: &Proof) -> bool {
    let Proof::Transcript { format, path, session_id } = proof else { return true };
    match db::run(&app.db, run_id).await {
        Ok(Some(r)) => {
            let same_id = r.native_session_id.as_deref() == Some(session_id.as_str());
            // codex has no transcript_path column value; its log is named for the session id.
            let same_path = match format {
                LogFormat::Claude => r.transcript_path.as_deref().map(std::path::Path::new) == Some(path.as_path()),
                LogFormat::Codex => true,
            };
            same_id && same_path
        }
        _ => false,
    }
}

/// Carry out a plan. Up to the first keystroke every give-up is `NotAttempted`; after it, every
/// give-up is `Unproven`. Read failures are errors, never empty screens.
pub(crate) async fn execute_delivery(
    app: &Arc<App>,
    client: &HerdrClient,
    run: &db::Run,
    bot: &db::Bot,
    text: &str,
    plan: Plan,
) -> anyhow::Result<Delivered> {
    let (pane, proof) = match plan {
        Plan::AgentPrompt { target } => {
            client
                .call_timeout("agent.prompt", json!({"target": target, "text": text}), Duration::from_secs(10))
                .await?;
            return Ok(Delivered::Submitted);
        }
        Plan::Type { pane, proof } => (pane, proof),
    };
    // Persist "this pane gets typed into" before the first keystroke (sol review round three #2).
    crate::lifecycle::remember_pane_typed(&run.id);
    if let Err(e) = db::set_pane_typed(&app.db, &run.id).await {
        anyhow::bail!("could not record runs.pane_typed for {} before typing into its pane: {e}", run.id);
    }
    let read = || async { client.pane_read(&pane, "recent-unwrapped", DELIVER_SCAN_LINES).await.map(|r| r.text) };

    // The box may have changed since the plan (someone typing in the terminal): look again.
    let before = read().await?;
    match box_state(&bot.kind, &before) {
        BoxState::Empty => {}
        BoxState::NonEmpty => return Ok(Delivered::NotAttempted { reason: "composer_busy", retry: true }),
        BoxState::Unready => return Ok(Delivered::NotAttempted { reason: "composer_unreadable", retry: true }),
    }
    let offset = match &proof {
        Proof::Transcript { path, .. } => transcript_len(path)?,
        Proof::EchoRow | Proof::Unverified => 0,
    };
    let baseline = evidence(&bot.kind, &proof, offset, &before, text)?;

    client.pane_send_text(&pane, text).await?;
    tokio::time::sleep(Duration::from_millis(TYPE_SETTLE_MS)).await;
    let mut seen = read().await?;
    if box_state(&bot.kind, &seen) == BoxState::Empty && evidence(&bot.kind, &proof, offset, &seen, text)? == baseline {
        tracing::warn!(run = %run.id, bot = %bot.name, "the paste did not reach the composer; pasting once more");
        client.pane_send_text(&pane, text).await?;
        tokio::time::sleep(Duration::from_millis(TYPE_SETTLE_MS)).await;
        seen = read().await?;
    }
    match box_state(&bot.kind, &seen) {
        BoxState::NonEmpty => {}
        BoxState::Empty if proof != Proof::Unverified && evidence(&bot.kind, &proof, offset, &seen, text)? > baseline => {
            return Ok(Delivered::Submitted);
        }
        BoxState::Empty => return Ok(Delivered::Unproven("nothing_typed")),
        BoxState::Unready => return Ok(Delivered::Unproven("composer_unreadable")),
    }

    client.pane_send_keys(&pane, &["Enter"]).await?;
    let mut pressed_again = false;
    for _ in 0..SUBMIT_CHECKS {
        tokio::time::sleep(Duration::from_millis(SUBMIT_SETTLE_MS)).await;
        let now = read().await?;
        if !same_session(app, &run.id, &proof).await {
            tracing::warn!(run = %run.id, bot = %bot.name, "the session changed while delivering; the transcript proof no longer applies");
            return Ok(Delivered::Unproven("session_changed"));
        }
        match box_state(&bot.kind, &now) {
            // No evidence to wait for: the box took the paste and emptied on Enter. That is all
            // that can be said, and it is said as `Unverified`, not as a proven delivery.
            BoxState::Empty if proof == Proof::Unverified => {
                tracing::warn!(run = %run.id, bot = %bot.name, "prompt typed and submitted; no lossless evidence on this run");
                return Ok(Delivered::Unverified);
            }
            BoxState::Empty if evidence(&bot.kind, &proof, offset, &now, text)? > baseline => {
                tracing::info!(run = %run.id, bot = %bot.name, proof = ?proof, "prompt typed into the pane and proven submitted");
                return Ok(Delivered::Submitted);
            }
            BoxState::NonEmpty if !pressed_again => {
                tracing::warn!(run = %run.id, bot = %bot.name, "prompt still in the composer after Enter; pressing Enter again");
                client.pane_send_keys(&pane, &["Enter"]).await?;
                pressed_again = true;
            }
            _ => {}
        }
    }
    let why = match box_state(&bot.kind, &read().await?) {
        BoxState::NonEmpty => "still_in_box",
        BoxState::Unready => "composer_unreadable",
        BoxState::Empty => "not_proven_submitted",
    };
    tracing::warn!(run = %run.id, bot = %bot.name, reason = why, "could not prove the prompt was submitted");
    Ok(Delivered::Unproven(why))
}

/// Plan and execute in one go, for callers that have no turn to hold back (queue flush, resend).
pub(crate) async fn deliver_prompt(
    app: &Arc<App>,
    client: &HerdrClient,
    run: &db::Run,
    bot: &db::Bot,
    text: &str,
    force_pane: bool,
) -> anyhow::Result<Delivered> {
    match plan_delivery(app, client, run, bot, text, force_pane).await? {
        Ok(plan) => execute_delivery(app, client, run, bot, text, plan).await,
        Err(not) => Ok(not),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const RULE: &str = "─────────────────────────────────────────────";

    fn screen(transcript: &[&str], box_rows: &[&str]) -> String {
        let mut s = String::new();
        for l in transcript {
            s.push_str(l);
            s.push('\n');
        }
        s.push_str("✻ Crunching… (3s · esc to interrupt)\n");
        s.push_str(RULE);
        s.push('\n');
        match box_rows.split_first() {
            None => s.push_str("❯\n"),
            Some((first, rest)) => {
                s.push_str(&format!("❯ {first}\n"));
                for r in rest {
                    s.push_str(&format!("  {r}\n"));
                }
            }
        }
        s.push_str(RULE);
        s.push('\n');
        s.push_str("  user. | web | OP5 61% | 5h:53% | 7d:95%\n");
        s
    }

    /// 只有已知的空框形狀才算空：任何額外位元組、任何續行（含空白列）都是非空（第七輪 #1）。
    #[test]
    fn only_the_exact_empty_shape_is_an_empty_box() {
        let empty = screen(&["⏺ 先前的回覆"], &[]);
        assert_eq!(box_state("claude", &empty), BoxState::Empty);
        let table: &[(&str, &str)] = &[
            ("❯  ", "marker 後多一個空白"),
            ("❯ x", "有字"),
            ("❯   ", "只有空白"),
        ];
        for (row, why) in table {
            let s = empty.replacen("❯\n", &format!("{row}\n"), 1);
            assert_eq!(box_state("claude", &s), BoxState::NonEmpty, "{why}");
        }
        let blank_second = empty.replacen("❯\n", "❯\n  \n", 1);
        assert_eq!(box_state("claude", &blank_second), BoxState::NonEmpty, "空白第二行");
        let second_line = empty.replacen("❯\n", "❯\n  草稿\n", 1);
        assert_eq!(box_state("claude", &second_line), BoxState::NonEmpty, "第二行有字");
        let suggestion = empty.replacen("❯\n", "❯ Try \"fix lint errors\"\n", 1);
        assert_eq!(box_state("claude", &suggestion), BoxState::NonEmpty, "建議句也不是空框");
        assert_eq!(box_state("claude", "Select login method:\n  1. Claude account\n"), BoxState::Unready);
    }

    /// 一列回音只給「保證不會折行」的單行：要知道 pane 寬度，而且保守估寬後放得下。
    #[test]
    fn one_echo_row_is_only_for_a_prompt_that_provably_fits() {
        let w = Some(80);
        let table: &[(&str, Option<u32>, bool, &str)] = &[
            ("Reply with PONG", w, true, "一般單行"),
            ("加一個功能除了按 SKU 之外", w, true, "CJK 單行，保守估寬仍放得下"),
            ("Reply with PONG", None, false, "不知道寬度就證明不了不折行"),
            ("Reply with PONG", Some(20), false, "窄 pane 放不下"),
            (&"x".repeat(80), w, false, "跟 pane 一樣長"),
            ("line one\nline two", w, false, "硬換行"),
            ("trailing  ", w, false, "行尾空白"),
            ("  indented", w, false, "開頭空白"),
            ("tab\there", w, false, "tab"),
            ("family 👨\u{200D}👩", w, false, "ZWJ"),
            ("heart ❤\u{FE0F}", w, false, "變體選擇符"),
            ("e\u{0301}clair", w, false, "組合字元"),
        ];
        for (text, cols, want, why) in table {
            assert_eq!(provable_by_echo_row(text, 2, *cols), *want, "{why}");
        }
        // 剛好放得下／差一欄的邊界。
        let fits = "x".repeat(80 - 2 - ROW_SAFETY_COLS);
        assert!(provable_by_echo_row(&fits, 2, Some(80)));
        assert!(!provable_by_echo_row(&format!("{fits}x"), 2, Some(80)));
    }

    #[test]
    fn a_row_with_a_continuation_under_it_proves_nothing() {
        let ambiguous = screen(&["❯ ab", "  cd"], &[]);
        assert_eq!(echo_row_hits("claude", &ambiguous, "ab"), 0);
        assert_eq!(echo_row_hits("claude", &ambiguous, "abcd"), 0);
        assert_eq!(echo_row_hits("claude", &screen(&["❯ ab", "⏺ 好"], &[]), "ab"), 1);
        assert_eq!(echo_row_hits("claude", &screen(&["> Reply with PONG"], &[]), "Reply with PONG"), 1, "`> ` 變體");
        assert_eq!(echo_row_hits("claude", &screen(&["❯  Reply with PONG"], &[]), "Reply with PONG"), 0, "多一個空白");
        // 回覆若以兩格縮排的純文字開頭，會被當成續行 → 假陰性（Unproven），不會假陽性。
        assert_eq!(echo_row_hits("claude", &screen(&["❯ ab", "  plain reply text"], &[]), "ab"), 0);
    }

    fn user_entry(content: Value) -> String {
        json!({"type": "user", "message": {"role": "user", "content": content}}).to_string()
    }

    /// transcript 只看基準點之後新增的位元組；逐字比對，截半的 UTF-8／JSON 行不會誤判。
    #[test]
    fn the_transcript_is_read_from_the_baseline_offset_and_compared_byte_for_byte() {
        let dir = std::env::temp_dir().join(format!("am-transcript-{}", db::ulid()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("t.jsonl");
        let code = "修這段：\n\nfn main() {\n    println!(\"hi\");  \n}";
        // 舊的同一句已經在基準點之前：不算。
        std::fs::write(&path, format!("{}\n", user_entry(json!(code)))).unwrap();
        let offset = transcript_len(&path).unwrap();
        assert_eq!(transcript_hits_since(&path, offset, code).unwrap(), 0);
        let long: String = (0..2000).map(|i| format!("  line {i}\n")).collect();
        let appended = [
            user_entry(json!("修這段：\nfn main() {\nprintln!(\"hi\");\n}")),
            user_entry(json!([{"type": "tool_result", "content": code}])),
            json!({"type": "user", "isMeta": true, "message": {"content": code}}).to_string(),
            user_entry(json!(code)),
            user_entry(json!([{"type": "text", "text": long.clone()}])),
        ];
        let mut f = std::fs::OpenOptions::new().append(true).open(&path).unwrap();
        use std::io::Write as _;
        writeln!(f, "{}", appended.join("\n")).unwrap();
        assert_eq!(transcript_hits_since(&path, offset, code).unwrap(), 1);
        assert_eq!(transcript_hits_since(&path, offset, &code.replace("  \n", "\n")).unwrap(), 0, "行尾兩空白是內容");
        assert_eq!(transcript_hits_since(&path, offset, &long).unwrap(), 1, "2k 行");
        // 基準點落在一個多位元組字元中間：第一段解析失敗被略過，後面的完整行照常。
        assert!(transcript_hits_since(&path, offset + 1, code).unwrap() <= 1);
        // 檔案被換掉變短：不是基準點那一份，回錯。
        std::fs::write(&path, "").unwrap();
        assert!(transcript_hits_since(&path, offset, code).is_err());
        assert!(transcript_hits_since(&dir.join("missing.jsonl"), 0, code).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn inputs<'a>(kind: &'a str, local: bool, session: Option<&'a str>, path: Option<&'a str>, codex_log: Option<std::path::PathBuf>, cols: Option<u32>) -> ProofInputs<'a> {
        ProofInputs { kind, host_is_local: local, hooks: true, session_id: session, transcript_path: path, codex_log, pane_cols: cols }
    }

    /// 證據矩陣（SPEC §4.4a）：provider × 本機／遠端 × 單行／多行 → 用哪種證據，或 unverified。
    #[test]
    fn the_evidence_matrix() {
        let dir = std::env::temp_dir().join(format!("am-proof-{}", db::ulid()));
        std::fs::create_dir_all(&dir).unwrap();
        let t = dir.join("t.jsonl");
        std::fs::write(&t, "").unwrap();
        let tp = t.to_str().unwrap();
        let clog = dir.join("rollout-x-sess.jsonl");
        std::fs::write(&clog, "").unwrap();
        let claude_t = Proof::Transcript { format: LogFormat::Claude, path: t.clone(), session_id: "s1".into() };
        let codex_t = Proof::Transcript { format: LogFormat::Codex, path: clog.clone(), session_id: "s1".into() };
        let long_line = "x".repeat(5000);
        let multi = "a\nb";
        let w = Some(80);
        let table: Vec<(&str, ProofInputs, &str, Result<Proof, Delivered>)> = vec![
            ("claude 本機 單行", inputs("claude", true, Some("s1"), Some(tp), None, w), "go", Ok(claude_t.clone())),
            ("claude 本機 多行", inputs("claude", true, Some("s1"), Some(tp), None, w), multi, Ok(claude_t.clone())),
            ("claude 本機 極窄＋長單行", inputs("claude", true, Some("s1"), Some(tp), None, Some(20)), &long_line, Ok(claude_t.clone())),
            ("claude 本機 還沒回報 session", inputs("claude", true, None, None, None, w), multi,
                Err(Delivered::NotAttempted { reason: "transcript_not_ready", retry: true })),
            ("claude 遠端 單行放得下", inputs("claude", false, Some("s1"), Some(tp), None, w), "go", Ok(Proof::EchoRow)),
            ("claude 遠端 多行", inputs("claude", false, Some("s1"), Some(tp), None, w), multi, Ok(Proof::Unverified)),
            ("codex 本機 有 rollout 單行", inputs("codex", true, Some("s1"), None, Some(clog.clone()), w), "go", Ok(codex_t.clone())),
            ("codex 本機 有 rollout 多行", inputs("codex", true, Some("s1"), None, Some(clog.clone()), w), multi, Ok(codex_t)),
            ("codex 本機 還不知道 session 多行", inputs("codex", true, None, None, None, w), multi, Ok(Proof::Unverified)),
            ("codex 遠端 單行放得下", inputs("codex", false, Some("s1"), None, None, w), "go", Ok(Proof::EchoRow)),
            ("codex 遠端 多行", inputs("codex", false, Some("s1"), None, None, w), multi, Ok(Proof::Unverified)),
            ("grok 本機 單行放得下", inputs("grok", true, None, None, None, w), "go", Ok(Proof::EchoRow)),
            ("grok 本機 多行", inputs("grok", true, None, None, None, w), multi, Ok(Proof::Unverified)),
            ("grok 本機 量不到寬度的單行", inputs("grok", true, None, None, None, None), "go", Ok(Proof::Unverified)),
            ("grok 遠端 多行", inputs("grok", false, None, None, None, w), multi, Ok(Proof::Unverified)),
        ];
        for (why, i, text, want) in table {
            assert_eq!(choose_proof(&i, text), want, "{why}");
        }
        // hooks 關掉的 claude 永遠不會回報 transcript：不等，直接打並標 unverified。
        let no_hooks = ProofInputs { hooks: false, ..inputs("claude", true, None, None, None, w) };
        assert_eq!(choose_proof(&no_hooks, multi), Ok(Proof::Unverified));
        let huge = "x".repeat(MAX_PROVABLE_CHARS + 1);
        assert_eq!(
            choose_proof(&inputs("claude", true, Some("s1"), Some(tp), None, w), &huge),
            Err(Delivered::NotAttempted { reason: "prompt_too_long_to_prove", retry: false }),
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// codex rollout：只算頂層 response_item 的 user 訊息；compacted 重播、非 input_text、developer 都不算；逐字比對。
    #[test]
    fn codex_rollout_user_entries_are_read_exactly() {
        let dir = std::env::temp_dir().join(format!("am-codex-{}", db::ulid()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("rollout.jsonl");
        let text = "第一行：先看報告\n    縮排的第二行  \n";
        let entry = |role: &str, parts: Value| json!({"type": "response_item", "payload": {"type": "message", "role": role, "content": parts}}).to_string();
        std::fs::write(&path, format!("{}\n", json!({"type": "session_meta", "payload": {"id": "s1"}}))).unwrap();
        let offset = transcript_len(&path).unwrap();
        let lines = [
            json!({"type": "compacted", "payload": {"replacement_history": [{"type": "message", "role": "user", "content": [{"type": "input_text", "text": text}]}]}}).to_string(),
            entry("developer", json!([{"type": "input_text", "text": text}])),
            entry("user", json!([{"type": "input_text", "text": text}, {"type": "input_image", "image_url": "x"}])),
            entry("user", json!([{"type": "input_text", "text": text.trim_end()}])),
            entry("user", json!([{"type": "input_text", "text": text}])),
        ];
        let mut f = std::fs::OpenOptions::new().append(true).open(&path).unwrap();
        use std::io::Write as _;
        writeln!(f, "{}", lines.join("\n")).unwrap();
        assert_eq!(log_hits_since(LogFormat::Codex, &path, offset, text).unwrap(), 1, "只有最後那筆一字不差");
        assert_eq!(log_hits_since(LogFormat::Claude, &path, offset, text).unwrap(), 0, "格式不同就不認");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 只認「檔名就是這個 session」的 rollout，最新的日期資料夾先找。
    #[test]
    fn the_codex_rollout_is_found_by_its_session_id() {
        let home = std::env::temp_dir().join(format!("am-codex-home-{}", db::ulid()));
        let old = home.join("sessions/2026/09/01");
        let new = home.join("sessions/2026/09/14");
        std::fs::create_dir_all(&old).unwrap();
        std::fs::create_dir_all(&new).unwrap();
        std::fs::write(old.join("rollout-2026-09-01T00-00-00-01a0aaaa-0000.jsonl"), "").unwrap();
        let want = new.join("rollout-2026-09-14T15-49-57-01a09ee4-eabb-7682-b363-941a606ed002.jsonl");
        std::fs::write(&want, "").unwrap();
        std::fs::write(new.join("rollout-2026-09-14T16-00-00-01a09ee4-eabb-7682-b363-941a606ed0029.jsonl"), "").unwrap();
        assert_eq!(codex_session_log(&home, "01a09ee4-eabb-7682-b363-941a606ed002"), Some(want));
        assert_eq!(codex_session_log(&home, "01a0aaaa-0000"), Some(old.join("rollout-2026-09-01T00-00-00-01a0aaaa-0000.jsonl")));
        assert_eq!(codex_session_log(&home, "nope"), None);
        assert_eq!(codex_session_log(&home, "../../etc"), None, "不接受會跑出目錄的 id");
        let _ = std::fs::remove_dir_all(&home);
    }
}

#[cfg(test)]
mod api_tests {
    //! Through `prompt()` itself: what reaches the database when nothing could be sent.
    use super::*;
    use crate::testing as tt;

    async fn idle_bot(env: &tt::Env, kind: &str) -> (String, String, String) {
        let app = &env.app;
        let bot_id = db::ulid();
        sqlx::query(
            "INSERT INTO bots (id, project_id, name, kind, args_json, autostart, inject_hooks, hook_token, created_at)
             VALUES (?,?,'api-bot',?,'[]',0,1,'tok',?)",
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
            "INSERT INTO runs (id, bot_id, state, agent_status, workspace_id, pane_id, agent_name, herdr_session, pane_typed, started_at)
             VALUES (?,?,'running','idle','ws-1','pane-api','api-bot','test',1,?)",
        )
        .bind(&run_id)
        .bind(&bot_id)
        .bind(db::now())
        .execute(&app.db)
        .await
        .unwrap();
        (bot_id, conv, run_id)
    }

    async fn turns(app: &Arc<App>, conv: &str) -> i64 {
        sqlx::query_scalar("SELECT COUNT(*) FROM turns WHERE conversation_id = ?").bind(conv).fetch_one(&app.db).await.unwrap()
    }

    /// 框裡有字：回可重試的 409，而且**沒有**建立 turn；框清空後同一個 request id 再送就成功（第七輪 #2）。
    #[tokio::test]
    async fn a_busy_box_is_a_retryable_409_with_no_turn_and_the_retry_goes_through() {
        let env = tt::env().await;
        let app = env.app.clone();
        let (bot_id, conv, _run) = idle_bot(&env, "claude").await;
        env.herdr.live_pane("pane-api", crate::testing::LivePane { composer: vec!["草稿".into()], width: Some(120), ..Default::default() });

        match prompt(&app, &bot_id, "Reply with PONG please", "crid-1").await {
            Err(LcError::Conflict(v)) => {
                assert_eq!(v.get("reason").and_then(Value::as_str), Some("composer_busy"));
                assert_eq!(v.get("retryable").and_then(Value::as_bool), Some(true));
            }
            other => panic!("expected a retryable 409, got {:?}", other.map(|o| o.delivery)),
        }
        assert_eq!(turns(&app, &conv).await, 0, "沒送出就沒有 in-flight unknown turn");

        env.herdr.live_pane("pane-api", crate::testing::LivePane { width: Some(120), ..Default::default() });
        let out = prompt(&app, &bot_id, "Reply with PONG please", "crid-1").await.unwrap();
        assert_eq!(out.delivery, "ok");
        assert_eq!(turns(&app, &conv).await, 1);
    }

    /// grok 多行沒有無損證據：照樣打字送出，回 200 `unverified`，turn 留下「要人工核對」的標記（不是 unknown）。
    #[tokio::test]
    async fn a_grok_multi_line_prompt_is_sent_and_marked_unverified() {
        let env = tt::env().await;
        let app = env.app.clone();
        let (bot_id, conv, _run) = idle_bot(&env, "grok").await;
        env.herdr.live_pane("pane-api", crate::testing::LivePane { width: Some(120), ..Default::default() });

        let out = prompt(&app, &bot_id, "第一行\n第二行", "crid-2").await.unwrap();
        assert_eq!(out.delivery, "unverified");
        assert_eq!(turns(&app, &conv).await, 1);
        let (delivery, verified): (String, i64) = sqlx::query_as("SELECT delivery, delivery_verified FROM turns WHERE id = ?")
            .bind(&out.turn_id)
            .fetch_one(&app.db)
            .await
            .unwrap();
        assert_eq!((delivery.as_str(), verified), ("ok", 0), "不是 unknown，而是帶標記的已送出");
        assert_eq!(env.herdr.methods().iter().filter(|m| *m == "pane.send_text").count(), 1);
        // 同一個 request id 再問一次，答案一樣是 unverified。
        assert_eq!(prompt(&app, &bot_id, "第一行\n第二行", "crid-2").await.unwrap().delivery, "unverified");
    }
}
