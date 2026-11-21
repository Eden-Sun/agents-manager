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

#[cfg(all(test, feature = "daemon-test-harness"))]
use crate::state::App;
use super::*;
use std::time::Duration;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Delivered {
    /// 打字進 pane，而且有無損證據（transcript／rollout／一列回音）證明它進了輸入框。
    Submitted,
    /// 交給 herdr `agent.prompt`。對方回 ok，但**沒有任何證據**說它真的進了輸入框
    /// （2026-09-14 wits-c1-op-xh：回 ok、字沒進去）。證據面記為未驗證，重送照舊允許
    /// （AGM 2026-09-16 裁示：證據與重送是兩件事）。
    Handed,
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

/// 一則送達要記下的兩件事，刻意分開（AGM 2026-09-16 裁示，review 第 3 條）：
/// * `verified` 只講**證據**——有沒有無損證據證明它進了對方的輸入框／session。
/// * `auto_resend` 只講**能不能自動重送**——打過字但證不明的那條路重送會重複派工，所以關掉；
///   `agent.prompt` 沒有證據但重送是安全的（沒進去才會重送），所以開著。
///   有證據的（`Submitted`）也關掉：證據就是「它已經進了 session」，stall watchdog 在畫面上找不到它
///   （長段貼上被 TUI 摺成 `[Pasted text …]`）時再打一次，只會讓 agent 做兩次（review3 c3 M4）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DeliveryRecord {
    /// 寫進 `turns.delivery`（CHECK 只認 pending/ok/unknown/failed）。
    pub stored: &'static str,
    pub verified: bool,
    pub auto_resend: bool,
}

impl Delivered {
    /// `None` = 這個結果不會寫 delivery（`NotAttempted` 的 turn 會被撤回）。
    pub fn record(&self) -> Option<DeliveryRecord> {
        match self {
            Delivered::Submitted => Some(DeliveryRecord { stored: "ok", verified: true, auto_resend: false }),
            Delivered::Handed => Some(DeliveryRecord { stored: "ok", verified: false, auto_resend: true }),
            Delivered::Unverified => Some(DeliveryRecord { stored: "ok", verified: false, auto_resend: false }),
            Delivered::Unproven(_) => Some(DeliveryRecord { stored: "unknown", verified: false, auto_resend: true }),
            Delivered::NotAttempted { .. } => None,
        }
    }
}

/// Which agent wrote a session log, i.e. how to read a user entry out of it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogFormat {
    /// claude `projects/<cwd>/<session>.jsonl`: `{"type":"user","message":{"content":…}}`.
    Claude,
    /// codex `sessions/YYYY/MM/DD/rollout-…-<session>.jsonl`:
    /// `{"type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text",…}]}}`.
    Codex,
    /// agy `~/.gemini/antigravity-cli/brain/<conversation>/.system_generated/logs/transcript_full.jsonl`:
    /// `{"type":"USER_INPUT","content":"<USER_REQUEST>\n…\n</USER_REQUEST>…"}`（原文被標籤包著，[`crate::agy_support::user_text`] 剝掉）。
    Agy,
}

/// The evidence a delivery will be judged by.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Proof {
    /// Count single echo rows equal to the prompt.
    EchoRow,
    /// Count exact user entries appended to the agent's own session log after the baseline offset,
    /// while the run still points at this session.
    Transcript { format: LogFormat, path: std::path::PathBuf, session_id: String },
    /// No lossless evidence on this run: type, submit, and report `Unverified`.
    Unverified,
}

/// 把框裡那句送出去的鍵。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Submit {
    /// 一般送出：框是空的、agent 閒著，Enter 就是送出。
    Enter,
    /// 插隊送出（issue #103）：claude 2.1.275 的 send-now 鍵，打斷目前這一回合並把排著的訊息一次送出。
    /// 用 `ctrl+x ctrl+s` 不用 `ctrl+enter`——終端對 `ctrl+enter` 的支援不一致（issue #103 的建議）。
    ///
    /// 鍵名對 herdr 0.8.2 實測過（2026-09-18，隔離的 scratch pane 跑 `stty -ixon && cat -v`）：
    /// `pane.send_keys ["ctrl+x","ctrl+s"]` 進到 tty 是 `^X^S`（0x18 0x13）。預設的 `cat -v` 只看到 `^X^X`，
    /// 那是 tty 的 IXON 把 `ctrl+s` 當成 XOFF 吃掉——claude 的 TUI 是 raw mode，沒有這層。
    SendNow,
}

impl Submit {
    pub fn keys(self) -> &'static [&'static str] {
        match self {
            Submit::Enter => &["Enter"],
            Submit::SendNow => &["ctrl+x", "ctrl+s"],
        }
    }
}

/// How the prompt will be delivered, decided before anything is written.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Plan {
    AgentPrompt { target: String },
    Type { pane: String, proof: Proof, submit: Submit },
}

impl Plan {
    /// 換掉送出鍵（issue #103）。`AgentPrompt` 那條路沒有鍵可以按——插隊送出一律帶 `force_pane`，
    /// 所以計畫必然是 `Type`；真的拿到 `AgentPrompt` 時原樣回傳，讓它走一般送出而不是悄悄插隊。
    pub fn submitting_with(self, submit: Submit) -> Self {
        match self {
            Plan::Type { pane, proof, .. } => Plan::Type { pane, proof, submit },
            other => other,
        }
    }
}

const TYPE_SETTLE_MS: u64 = 700;
pub const SUBMIT_SETTLE_MS: u64 = 1200;
pub const SUBMIT_CHECKS: u32 = 3;
pub const DELIVER_SCAN_LINES: u32 = 400;
/// The `pane.read` source for delivery scans. herdr only knows `visible | recent | recent_unwrapped |
/// detection` — the hyphenated spelling is an `invalid_request` (4713c5c P0).
pub const SCAN_SOURCE: &str = "recent_unwrapped";
/// Longest prompt the daemon will type and try to prove.
pub const MAX_PROVABLE_CHARS: usize = 200_000;
/// Columns kept free at the right edge when deciding a prompt fits on one row: the TUI's own
/// padding and cursor cell. Generous on purpose — a false "does not fit" only costs a deferral.
const ROW_SAFETY_COLS: usize = 6;

/// The echo markers a kind draws at the start of a message; matched as exact row prefixes.
pub fn echo_markers(kind: &str) -> &'static [&'static str] {
    match kind {
        "claude" => &["❯ ", "> "],
        "grok" => &["❯ "],
        "codex" => &["› "],
        "agy" => &["> "],
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
pub fn provable_by_echo_row(text: &str, marker_cols: usize, pane_cols: Option<u32>) -> bool {
    let Some(cols) = pane_cols else { return false };
    !text.is_empty()
        && !text.contains('\n')
        && !text.contains('\r')
        && text == text.trim()
        && !text.chars().any(uncertain_char)
        && marker_cols + max_cols(text) + ROW_SAFETY_COLS <= cols as usize
}

/// A full-width rule the TUI draws above and below claude's current composer.
#[allow(dead_code)]
fn is_rule_row(row: &str) -> bool {
    let t = row.trim();
    !t.is_empty() && t.chars().all(|c| "─━".contains(c))
}

#[allow(unused_imports)]
pub use crate::composer_parse::{
    blank_codex_particles, composer_row, composer_tail, is_box_bottom,
    is_hint_cell, plain_without_hints, prompt_suggestion, strip_ansi, styled_cells, styled_chars,
    BoxState, Cell, ComposerRow, SUGGESTION_MAX_CHARS,
};

/// 選單游標和 Claude 輸入列都用 ❯；完整輸入框中的編號草稿仍是草稿。
pub(super) fn claude_open_choice_menu(screen: &str) -> bool {
    let plain = strip_ansi(screen);
    let lines: Vec<&str> = plain.lines().collect();
    if let Some(marker) = lines.iter().rposition(|line| line.trim_start().starts_with('❯')) {
        let framed = marker > 0
            && is_rule_row(lines[marker - 1])
            && lines[marker + 1..].iter().any(|line| is_rule_row(line));
        if framed {
            return false;
        }
    }
    crate::tui_prompts::awaits_menu_choice(&plain)
}

/// 額度警告會保留舊的 composer 片段；它存在時不能借那格空框認定已 ready／已送出。
pub fn box_state(kind: &str, screen: &str) -> BoxState {
    if kind == "claude" && (crate::tui_prompts::cold_resume_choice(screen).is_some() || claude_open_choice_menu(screen)) {
        BoxState::Unready
    } else if kind == "agy" && crate::agy_screen::blocking_dialog(screen).is_some() {
        BoxState::Unready
    } else {
        crate::composer_parse::box_state(kind, screen)
    }
}

pub fn box_state_within(kind: &str, screen: &str, tail: usize) -> BoxState {
    if kind == "claude" && (crate::tui_prompts::cold_resume_choice(screen).is_some() || claude_open_choice_menu(screen)) {
        BoxState::Unready
    } else if kind == "agy" && crate::agy_screen::blocking_dialog(screen).is_some() {
        BoxState::Unready
    } else {
        crate::composer_parse::box_state_within(kind, screen, tail)
    }
}

fn continuation_row(row: &str) -> bool {
    row.strip_prefix("  ").map(|rest| !rest.trim().is_empty()).unwrap_or(false)
        && !row.trim_start().starts_with(['⏺', '✻', '⎿', '●', '─', '│'])
}

/// How many single, un-continued echo rows above the composer say exactly `text`.
pub fn echo_row_hits(kind: &str, screen: &str, text: &str) -> usize {
    let lines: Vec<&str> = screen.lines().collect();
    let end = composer_row(kind, &lines).unwrap_or(lines.len());
    let markers = echo_markers(kind);
    (0..end)
        .filter(|&i| {
            let row = lines[i];
            let row = row.trim_end();
            let exact = if kind == "grok" {
                let plain = crate::lifecycle::screen::strip_grok_decor(row);
                markers.iter().any(|m| plain.strip_prefix(m) == Some(text))
            } else {
                markers.iter().any(|m| row.strip_prefix(m) == Some(text))
            };
            let continued = i + 1 < end && continuation_row(lines[i + 1]);
            exact && !continued
        })
        .count()
}

/// The text of one session-log line if it is a user message a person typed.
pub fn log_user_text(format: LogFormat, line: &str) -> Option<String> {
    match format {
        LogFormat::Claude => transcript_user_text(line),
        LogFormat::Codex => codex_user_text(line),
        LogFormat::Agy => crate::agy_support::user_text(line),
    }
}

/// codex rollout: only a top-level `response_item` user message counts. `compacted` entries replay
/// old history inside `replacement_history` and are not new messages; parts other than
/// `input_text` (images, files) make the entry something other than our typed prompt.
pub fn codex_user_text(line: &str) -> Option<String> {
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

pub fn transcript_user_text(line: &str) -> Option<String> {
    let v: Value = serde_json::from_str(line).ok()?;
    super::claude_child_log::user_prompt_raw(&v)
}

pub fn transcript_len(path: &std::path::Path) -> std::io::Result<u64> {
    Ok(std::fs::metadata(path)?.len())
}

/// Exact user entries for `text` in the bytes of `path` from `offset` on. Only what was appended
/// after the baseline is read, so an old identical message sliding out of any window cannot hide
/// a new one, and the cost is the new bytes only. A partial first line fails to parse and is
/// skipped; a partial last line is simply not complete yet. `Err` = unreadable, never zero.
#[cfg(all(test, feature = "daemon-test-harness"))]
pub fn transcript_hits_since(path: &std::path::Path, offset: u64, text: &str) -> std::io::Result<usize> {
    log_hits_since(LogFormat::Claude, path, offset, text)
}

pub fn log_hits_since(format: LogFormat, path: &std::path::Path, offset: u64, text: &str) -> std::io::Result<usize> {
    let buf = crate::transcript_read::read_since(path, offset, "transcript")?;
    let body = String::from_utf8_lossy(&buf);
    let ours = |t: &String| match format {
        LogFormat::Claude => super::pasted_content::is_sent(t, text),
        LogFormat::Codex => t == text,
        LogFormat::Agy => t.trim() == text.trim(),
    };
    Ok(body.lines().filter_map(|l| log_user_text(format, l)).filter(ours).count())
}

/// Find the rollout codex writes for `session_id` under `codex_home/sessions`.
///
/// The date tree is walked newest first, all of it — a session that has run for weeks is still
/// found. Inside a day, several files for the same session (a resume on the same day) resolve to the
/// newest by modification time. The winner is canonicalized and must still lie under the canonical
/// `sessions` root, so a symlinked entry cannot point the proof at some other file.
/// [`codex_session_log`] 放到 blocking pool：它整棵日期樹逐層 `read_dir`（用了好幾個月的 `~/.codex/sessions` 有上百個目錄），
/// 每次送 prompt 都會走一次，不能占 tokio worker。
pub async fn codex_session_log_async(codex_home: std::path::PathBuf, session_id: String) -> Option<std::path::PathBuf> {
    tokio::task::spawn_blocking(move || codex_session_log(&codex_home, &session_id)).await.ok().flatten()
}

pub fn codex_session_log(codex_home: &std::path::Path, session_id: &str) -> Option<std::path::PathBuf> {
    let id = session_id.trim();
    if id.is_empty() || !id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-') {
        return None;
    }
    let root = std::fs::canonicalize(codex_home.join("sessions")).ok()?;
    let suffix = format!("-{id}.jsonl");
    let children = |dir: &std::path::Path| -> Vec<std::path::PathBuf> {
        let mut v: Vec<std::path::PathBuf> = std::fs::read_dir(dir)
            .map(|rd| rd.filter_map(|e| e.ok()).map(|e| e.path()).collect())
            .unwrap_or_default();
        v.sort();
        v.reverse();
        v
    };
    for year in children(&root).into_iter().filter(|p| p.is_dir()) {
        for month in children(&year).into_iter().filter(|p| p.is_dir()) {
            for day in children(&month).into_iter().filter(|p| p.is_dir()) {
                let newest = children(&day)
                    .into_iter()
                    .filter(|p| {
                        p.file_name().and_then(|n| n.to_str()).map(|n| n.starts_with("rollout-") && n.ends_with(&suffix)).unwrap_or(false)
                    })
                    .filter_map(|p| std::fs::metadata(&p).and_then(|m| m.modified()).ok().map(|t| (t, p)))
                    .max_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(&b.1)))
                    .map(|(_, p)| p);
                if let Some(found) = newest {
                    let real = std::fs::canonicalize(&found).ok()?;
                    return (real.starts_with(&root) && real.is_file()).then_some(real);
                }
            }
        }
    }
    None
}

/// `CODEX_HOME` for this bot on the local host: identity env, then the bot's own env, else `~/.codex`.
pub async fn codex_home(app: &Arc<impl crate::tools::ToolsEnv + 'static>, bot: &db::Bot) -> Option<std::path::PathBuf> {
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
pub struct ProofInputs<'a> {
    pub kind: &'a str,
    pub host_is_local: bool,
    /// Whether the daemon injected hooks: without them claude never reports a transcript.
    pub hooks: bool,
    pub session_id: Option<&'a str>,
    pub transcript_path: Option<&'a str>,
    /// The codex rollout for `session_id`, when found.
    pub codex_log: Option<std::path::PathBuf>,
    /// This prompt has already waited for evidence that is on its way (a codex rollout not written
    /// yet); stop waiting and fall back to what is available now.
    pub waited_for_log: bool,
    pub pane_cols: Option<u32>,
}

/// The evidence matrix (SPEC §4.4a). Lossless evidence when it exists; otherwise the prompt is
/// still typed and reported `Unverified` — never refused, never re-sent.
pub fn choose_proof(i: &ProofInputs, text: &str) -> Result<Proof, Delivered> {
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
        ("agy", true) => {
            if let (Some(session), Some(path)) = (session, path) {
                if std::path::Path::new(path).is_file() {
                    return Ok(Proof::Transcript { format: LogFormat::Agy, path: path.into(), session_id: session.to_string() });
                }
            }
        }
        ("codex", true) => {
            if let Some(session) = session {
                match i.codex_log.as_ref().filter(|log| log.is_file()) {
                    Some(log) => {
                        return Ok(Proof::Transcript { format: LogFormat::Codex, path: log.clone(), session_id: session.to_string() });
                    }
                    None if !i.waited_for_log => {
                        return Err(Delivered::NotAttempted { reason: "codex_log_not_ready", retry: true });
                    }
                    None => {}
                }
            }
        }
        _ => {}
    }
    let marker_cols = echo_markers(i.kind).first().map(|m| m.chars().count()).unwrap_or(0);
    if marker_cols > 0 && provable_by_echo_row(text, marker_cols, i.pane_cols) {
        return Ok(Proof::EchoRow);
    }
    if i.kind == "claude" && i.host_is_local && i.hooks && (session.is_none() || path.is_none()) {
        return Err(Delivered::NotAttempted { reason: "transcript_not_ready", retry: true });
    }
    Ok(Proof::Unverified)
}

#[allow(unused_imports)]
pub use crate::composer_parse::{
    ansi_unsupported, read_styled, read_styled_snapshot, should_warn_plain_for_ansi, warn_due,
    PLAIN_FOR_ANSI_WARN_SECS,
};

/// Read the pane for the composer checks: styled when herdr can, plain when it cannot. A plain
/// read is the fail-closed fallback — no dim flags, so a placeholder simply reads as `NonEmpty`.
pub(super) async fn read_composer(client: &HerdrClient, pane: &str) -> anyhow::Result<String> {
    read_styled(client, pane, SCAN_SOURCE, DELIVER_SCAN_LINES).await
}

/// `agent.prompt` on an agent herdr has no session bound to answered ok while the text never
/// reached the pane (2026-09-14 wits-c1-op-xh).
async fn agent_prompt_usable(client: &HerdrClient, target: &str) -> bool {
    match client.agent_get(target).await {
        Ok(Some(info)) => info.agent_session.is_some(),
        _ => false,
    }
}

/// 打第一個字之前的輸入框判斷（計畫、準備、貼字前三處共用）。codex 的帳號安全提醒橫幅開著時框看起來是空的，但打進去的
/// 開頭數字會被當成選項吃掉（#782）：整則不送、可重試，第一次看到時通知人；不按 Esc 或任何鍵。
pub(super) async fn composer_ready(app: &impl LcHost, run: &db::Run, kind: &str, screen: &str) -> Result<(), Delivered> {
    let refreshed;
    let screen = if kind == "claude" && crate::tui_prompts::cold_resume_choice(screen).is_some() {
        let pending = || Delivered::NotAttempted { reason: super::claude_resume_menu::PENDING, retry: true };
        super::claude_resume_menu::accept_if_shown(app, run).await.map_err(|_| pending())?;
        let client = super::client_for_run(app, run).await.map_err(|_| pending())?;
        refreshed = read_composer(&client, run.pane_id.as_deref().ok_or_else(pending)?).await.map_err(|_| pending())?;
        &refreshed
    } else { screen };
    if kind == "codex" {
        if super::codex_hook_review::review_open(screen) {
            app.observe_codex_hook_review(run, screen).await;
            return Err(Delivered::NotAttempted { reason: super::codex_hook_review::REASON, retry: true });
        }
        let banner = super::codex_banner::blocks_typing(screen);
        super::codex_banner::observe(app, run, banner).await;
        if banner {
            return Err(Delivered::NotAttempted { reason: super::codex_banner::REASON, retry: true });
        }
        if codex_has_queued_followup(screen) {
            return Err(Delivered::NotAttempted { reason: "composer_busy", retry: true });
        }
    }
    if kind == "agy" {
        if let Some(dialog) = crate::agy_screen::blocking_dialog(screen) {
            return Err(Delivered::NotAttempted { reason: dialog.reason(), retry: true });
        }
    }
    match box_state(kind, screen) {
        BoxState::Empty => Ok(()),
        BoxState::NonEmpty => Err(Delivered::NotAttempted { reason: "composer_busy", retry: true }),
        BoxState::Unready => Err(Delivered::NotAttempted { reason: "composer_unreadable", retry: true }),
    }
}

/// Codex 0.171 keeps queued follow-ups above the composer, so the composer can be empty while a
/// prior question is still waiting to be sent automatically. Only inspect the queue block adjacent
/// to the current composer; old transcript rows must not block future delivery.
fn codex_has_queued_followup(screen: &str) -> bool {
    let lines: Vec<&str> = screen.lines().collect();
    let Some(composer) = crate::composer_parse::composer_row("codex", &lines) else { return false };
    let start = composer.saturating_sub(12);
    let rows: Vec<String> = lines[start..composer]
        .iter()
        .map(|row| crate::composer_parse::strip_ansi(row))
        .collect();
    let Some(header) = rows
        .iter()
        .rposition(|row| row.trim().trim_start_matches('•').trim() == "Queued follow-up inputs")
    else {
        return false;
    };
    rows[header + 1..].iter().any(|row| row.trim_start().starts_with('↳'))
}

/// Decide how to deliver, touching nothing: the route, the evidence and an empty box. Callers run
/// this **before** committing a turn, so a prompt that cannot be sent never becomes one.
pub async fn plan_delivery(
    app: &impl LcHost,
    client: &HerdrClient,
    run: &db::Run,
    bot: &db::Bot,
    text: &str,
    force_pane: bool,
    waited_for_log: bool,
) -> anyhow::Result<Result<Plan, Delivered>> {
    let target = db::run_target(run, bot);
    if target.is_empty() {
        return Ok(Err(Delivered::NotAttempted { reason: "no_pane_to_type_into", retry: true }));
    }
    if super::dead_panes::pane_target_stale(client, run).await {
        return Ok(Err(Delivered::NotAttempted { reason: "pane_reused", retry: false }));
    }
    let marked = crate::lifecycle::pane_typed_memo(&run.id)
        || match db::pane_typed(app.db(), &run.id).await {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!(run = %run.id, error = %e, "could not read runs.pane_typed; assuming this pane needs typing");
                true
            }
        };
    let resumed_claude = bot.kind == "claude" && (run.resume_session_id.is_some() || run.resume_outcome.is_some());
    if !(force_pane || marked || resumed_claude || !agent_prompt_usable(client, &target).await) {
        return Ok(Ok(Plan::AgentPrompt { target }));
    }
    let Some(pane) = run.pane_id.as_deref().map(str::trim).filter(|p| !p.is_empty()).map(str::to_string) else {
        return Ok(Err(Delivered::NotAttempted { reason: "no_pane_to_type_into", retry: true }));
    };
    let host_is_local = match db::project(app.db(), &bot.project_id).await {
        Ok(p) => p.is_some_and(|p| p.host == LOCAL_HOST),
        Err(e) => {
            tracing::warn!(run = %run.id, error = %e, "cannot read the bot's host; not typing without knowing which evidence applies");
            return Ok(Err(Delivered::NotAttempted { reason: "host_unreadable", retry: true }));
        }
    };
    let transcript_path = if let Some(path) = run.transcript_path.as_deref() {
        if matches!(bot.kind.as_str(), "claude" | "agy")
            && host_is_local
            && !crate::lifecycle::s6_ports::StuckTurnServices::local_transcript_allowed(app, bot, path).await
        {
            None
        } else {
            Some(path)
        }
    } else {
        None
    };
    let pane_cols = client.pane_size(&pane).await.ok().flatten().map(|(w, _)| w);
    let codex_log = match (bot.kind.as_str(), host_is_local, run.native_session_id.as_deref()) {
        ("codex", true, Some(session)) => match crate::lifecycle::s6_ports::StuckTurnServices::codex_home(app, bot).await {
            Some(h) => codex_session_log_async(h, session.to_string()).await,
            None => None,
        },
        _ => None,
    };
    let inputs = ProofInputs {
        kind: &bot.kind,
        host_is_local,
        hooks: bot.inject_hooks != 0,
        session_id: run.native_session_id.as_deref(),
        transcript_path,
        codex_log,
        waited_for_log,
        pane_cols,
    };
    let proof = match choose_proof(&inputs, text) {
        Ok(p) => p,
        Err(not) => return Ok(Err(not)),
    };
    let mut screen = match read_composer(client, &pane).await {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!(run = %run.id, error = %e, "could not read the pane before typing");
            return Ok(Err(Delivered::NotAttempted { reason: "composer_unreadable", retry: true }));
        }
    };
    let mut ready = composer_ready(app, run, &bot.kind, &screen).await;
    if matches!(ready, Err(Delivered::NotAttempted { reason: "composer_unreadable", .. })) {
        let deadline = tokio::time::Instant::now() + Duration::from_millis(1600);
        while tokio::time::Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(200)).await;
            if let Ok(refreshed) = read_composer(client, &pane).await {
                screen = refreshed;
                ready = composer_ready(app, run, &bot.kind, &screen).await;
                if !matches!(ready, Err(Delivered::NotAttempted { reason: "composer_unreadable", .. })) {
                    break;
                }
            }
        }
    }
    Ok(ready.map(|()| Plan::Type { pane, proof, submit: Submit::Enter }))
}

/// Current evidence count for `proof`.
pub(super) fn evidence(kind: &str, proof: &Proof, offset: u64, screen: &str, text: &str) -> std::io::Result<usize> {
    match proof {
        Proof::EchoRow => {
            let plain: String = screen.lines().map(strip_ansi).collect::<Vec<_>>().join("\n");
            Ok(echo_row_hits(kind, &plain, text))
        }
        Proof::Transcript { format, path, .. } => log_hits_since(*format, path, offset, text),
        Proof::Unverified => Ok(0),
    }
}

/// 要不要再貼一次：只有在**看得出來第一次沒進去**的時候。
///
/// `Proof::Unverified` 的 `evidence` 恆為 0（＝恆等於 baseline），所以「證據沒長」對它永遠成立，
/// 條件會退化成「框看起來是空的就再貼」——而 Unverified 正是遠端／grok／codex 還沒回報 session
/// 這些最慢的情境：TUI 晚一幀重畫就會貼第二次，兩段文字接在一起送進 agent，而且因為是 Unverified，
/// 送出後框清空就回報成功，沒有任何跡象顯示送出去的字跟使用者看到的不一樣（review 2026-09-16）。
/// 沒有守門員時寧可不動第二次寫入。
fn should_repaste(proof: &Proof, box_empty: bool, evidence_grew: bool) -> bool {
    !matches!(proof, Proof::Unverified) && box_empty && !evidence_grew
}

/// Is the run still on the session the transcript proof was taken from?
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionMatch {
    Same,
    Changed,
    Unavailable,
}

pub async fn same_session(app: &impl crate::capabilities::Db, run_id: &str, proof: &Proof) -> SessionMatch {
    let Proof::Transcript { format, path, session_id } = proof else { return SessionMatch::Same };
    #[cfg(all(test, feature = "daemon-test-harness"))]
    if fail_session_read_for_test(run_id) {
        return SessionMatch::Unavailable;
    }
    match db::run(app.db(), run_id).await {
        Ok(Some(r)) => {
            let same_id = r.native_session_id.as_deref() == Some(session_id.as_str());
            let same_path = match format {
                LogFormat::Claude | LogFormat::Agy => r.transcript_path.as_deref().map(std::path::Path::new) == Some(path.as_path()),
                LogFormat::Codex => true,
            };
            if same_id && same_path { SessionMatch::Same } else { SessionMatch::Changed }
        }
        Ok(None) => SessionMatch::Changed,
        Err(_) => SessionMatch::Unavailable,
    }
}

#[cfg(all(test, feature = "daemon-test-harness"))]
fn session_read_failures() -> &'static std::sync::Mutex<std::collections::HashSet<String>> {
    static FAILURES: std::sync::OnceLock<std::sync::Mutex<std::collections::HashSet<String>>> = std::sync::OnceLock::new();
    FAILURES.get_or_init(Default::default)
}

#[cfg(all(test, feature = "daemon-test-harness"))]
pub(crate) fn fail_next_session_read_for_test(run_id: &str) {
    session_read_failures().lock().unwrap_or_else(|e| e.into_inner()).insert(run_id.to_string());
}

#[cfg(all(test, feature = "daemon-test-harness"))]
fn fail_session_read_for_test(run_id: &str) -> bool {
    session_read_failures().lock().unwrap_or_else(|e| e.into_inner()).remove(run_id)
}

/// Carry out a plan. Up to the first keystroke every give-up is `NotAttempted`; after it, every
/// give-up is `Unproven`. Read failures are errors, never empty screens.
pub async fn execute_delivery(
    app: &impl LcHost,
    client: &super::RunClient,
    run: &db::Run,
    bot: &db::Bot,
    text: &str,
    plan: Plan,
) -> anyhow::Result<Delivered> {
    match prepare_delivery(app, client, run, bot, text, plan).await {
        Ok(ready) => type_prepared(app, client, run, bot, text, ready).await,
        Err(not) => Ok(not),
    }
}

/// 按鍵前的準備都做完了：下一步就是第一個按鍵。
pub enum Ready {
    AgentPrompt { target: String },
    Type { pane: String, proof: Proof, submit: Submit, offset: u64, baseline: usize, structured: Option<crate::codex_history::Mark> },
}

/// [`execute_delivery`] 打第一個字之前的那一段：記下「這個 pane 要打字」、重看一次框、取證據基準。
/// 每一種放棄都是 `NotAttempted`（一個鍵都還沒按）。拆出來是給插隊送出用的（#120）：它要在這一段
/// 成功之後才收掉被打斷的那一回合——先收再被擋下的話，claude 其實還在跑，對話裡卻多一筆假的「被打斷」。
pub async fn prepare_delivery(
    app: &impl LcHost,
    client: &HerdrClient,
    run: &db::Run,
    bot: &db::Bot,
    text: &str,
    plan: Plan,
) -> Result<Ready, Delivered> {
    if super::dead_panes::pane_target_stale(client, run).await {
        return Err(Delivered::NotAttempted { reason: "pane_reused", retry: false });
    }
    let (pane, proof, submit) = match plan {
        Plan::AgentPrompt { target } => return Ok(Ready::AgentPrompt { target }),
        Plan::Type { pane, proof, submit } => (pane, proof, submit),
    };
    crate::lifecycle::remember_pane_typed(&run.id);
    if let Err(e) = db::set_pane_typed(app.db(), &run.id).await {
        tracing::warn!(run = %run.id, error = %e, "could not record runs.pane_typed before typing; nothing was typed");
        return Err(Delivered::NotAttempted { reason: "pane_typed_unwritable", retry: true });
    }

    let before = match read_composer(client, &pane).await {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!(run = %run.id, error = %e, "could not read the pane before typing");
            return Err(Delivered::NotAttempted { reason: "composer_unreadable", retry: true });
        }
    };
    composer_ready(app, run, &bot.kind, &before).await?;
    let offset = match &proof {
        Proof::Transcript { path, .. } => match transcript_len(path) {
            Ok(n) => n,
            Err(e) => {
                tracing::warn!(run = %run.id, path = %path.display(), error = %e, "evidence file unreadable before typing");
                return Err(Delivered::NotAttempted { reason: "transcript_unreadable", retry: true });
            }
        },
        Proof::EchoRow | Proof::Unverified => 0,
    };
    let baseline = match evidence(&bot.kind, &proof, offset, &before, text) {
        Ok(n) => n,
        Err(e) => {
            tracing::warn!(run = %run.id, error = %e, "could not take the evidence baseline before typing");
            return Err(Delivered::NotAttempted { reason: "transcript_unreadable", retry: true });
        }
    };
    let structured = app.codex_history_mark(bot, run).await;
    Ok(Ready::Type { pane, proof, submit, offset, baseline, structured })
}

/// [`execute_delivery`] 準備完成後的派送。`type_text` 貼字前仍會重讀 composer；那一刻看到草稿還是 `NotAttempted`。
pub async fn type_prepared(
    app: &impl LcHost,
    client: &super::RunClient,
    run: &db::Run,
    bot: &db::Bot,
    text: &str,
    ready: Ready,
) -> anyhow::Result<Delivered> {
    #[cfg(all(test, feature = "daemon-test-harness"))]
    super::race_point::hit("delivery_before_typing", &run.bot_id).await;
    match type_text(app, client, run, bot, text, ready).await? {
        Typing::Done(d) => Ok(d),
        Typing::Ready(t) => {
            press_submit(client, &t).await?;
            confirm_submitted(app, client, run, bot, text, &t).await
        }
    }
}

/// 字已經在框裡，下一步就是送出鍵。
pub struct Typed {
    pane: String,
    proof: Proof,
    submit: Submit,
    offset: u64,
    baseline: usize,
    /// codex app-server 歷史的基準（#749）；只用來把「證不出來」升級成 `Submitted`，永遠不拿來否定。
    structured: Option<crate::codex_history::Mark>,
}

/// [`type_text`] 停下來的地方。
pub enum Typing {
    /// 字在框裡，接下來是送出鍵。
    Ready(Typed),
    /// 送出鍵之前就有了結論（`Handed`；證據已經長出來＝`Submitted`；框是空的／讀不到＝`Unproven`）。
    Done(Delivered),
}

/// 剛貼進去的 `text` 讓框最多多高：預設的 24 列加上它自己的列數（上限 300，`read_composer` 只讀 400 列）。
pub(super) fn typed_tail(text: &str) -> usize {
    super::poller::COMPOSER_TAIL + (text.matches('\n').count() + 1).min(300)
}

/// 打字、看框（必要時重貼一次），停在送出鍵之前。插隊送出要在送出鍵上判斷有沒有打斷（#120），所以跟按鍵分開。
pub async fn type_text(
    app: &impl LcHost,
    client: &super::RunClient,
    run: &db::Run,
    bot: &db::Bot,
    text: &str,
    ready: Ready,
) -> anyhow::Result<Typing> {
    let (pane, proof, submit, offset, baseline, structured) = match ready {
        Ready::AgentPrompt { target } => {
            #[cfg(all(test, feature = "daemon-test-harness"))]
            super::race_point::hit("agent_prompt_before_rpc", &run.bot_id).await;
            if let Err(reason) =
                super::prompt::final_agent_prompt_guard(app, client, run, bot, &target).await
            {
                tracing::warn!(run = %run.id, reason, "agent.prompt final pane fence refused delivery");
                return Ok(Typing::Done(Delivered::NotAttempted {
                    reason,
                    retry: true,
                }));
            }
            #[cfg(all(test, feature = "daemon-test-harness"))]
            super::race_point::hit("agent_prompt_after_guard", &run.bot_id).await;
            if !app.hosts().is_current(&client.fence).await {
                return Ok(Typing::Done(Delivered::NotAttempted {
                    reason: "host_changed",
                    retry: true,
                }));
            }
            client
                .client
                .call_timeout(
                    "agent.prompt",
                    json!({"target": target, "text": text}),
                    Duration::from_secs(10),
                )
                .await?;
            return Ok(Typing::Done(Delivered::Handed));
        }
        Ready::Type { pane, proof, submit, offset, baseline, structured } => (pane, proof, submit, offset, baseline, structured),
    };
    let read = || read_composer(client, &pane);

    let before = match read().await {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!(run = %run.id, error = %e, "could not read the pane immediately before typing");
            return Ok(Typing::Done(Delivered::NotAttempted { reason: "composer_unreadable", retry: true }));
        }
    };
    if let Err(not) = composer_ready(app, run, &bot.kind, &before).await {
        return Ok(Typing::Done(not));
    }

    client.pane_send_text(&pane, text).await?;
    tokio::time::sleep(pane_settle(Duration::from_millis(TYPE_SETTLE_MS))).await;
    let mut seen = read().await?;
    if bot.kind == "claude" && crate::tui_prompts::cold_resume_choice(&seen).is_some() {
        if super::claude_resume_menu::accept_on(app, run, client).await.is_err() {
            return Ok(Typing::Done(Delivered::Unproven(super::claude_resume_menu::PENDING)));
        }
        seen = read().await?;
    }
    let mut box_empty = box_state_within(&bot.kind, &seen, typed_tail(text)) == BoxState::Empty;
    let mut evidence_grew = evidence(&bot.kind, &proof, offset, &seen, text)? > baseline;
    if should_repaste(&proof, box_empty, evidence_grew) {
        tokio::time::sleep(pane_settle(Duration::from_millis(TYPE_SETTLE_MS))).await;
        seen = read().await?;
        box_empty = box_state_within(&bot.kind, &seen, typed_tail(text)) == BoxState::Empty;
        evidence_grew = evidence(&bot.kind, &proof, offset, &seen, text)? > baseline;
    }
    if should_repaste(&proof, box_empty, evidence_grew) {
        tracing::warn!(run = %run.id, bot = %bot.name, "the paste did not reach the composer; pasting once more");
        client.pane_send_text(&pane, text).await?;
        tokio::time::sleep(pane_settle(Duration::from_millis(TYPE_SETTLE_MS))).await;
        seen = read().await?;
        if box_state_within(&bot.kind, &seen, typed_tail(text)) == BoxState::Empty {
            tokio::time::sleep(pane_settle(Duration::from_millis(TYPE_SETTLE_MS))).await;
            seen = read().await?;
        }
    } else if proof == Proof::Unverified && box_empty {
        tokio::time::sleep(pane_settle(Duration::from_millis(TYPE_SETTLE_MS))).await;
        seen = read().await?;
    }
    let state = box_state_within(&bot.kind, &seen, typed_tail(text));
    let truncated = match super::paste_check::lost_head(&bot.kind, &seen, text, typed_tail(text)) {
        Some(lost) if state == BoxState::NonEmpty => {
            lost.truncated(client.pane_viewport_rows(&pane).await.ok().flatten().map(|r| r as usize))
        }
        _ => false,
    };
    match state {
        BoxState::NonEmpty if truncated => {
            tracing::warn!(run = %run.id, bot = %bot.name, "the composer lost the head of the paste; clearing it instead of sending half");
            client.pane_send_keys(&pane, &["ctrl+c"]).await?;
            tokio::time::sleep(pane_settle(Duration::from_millis(TYPE_SETTLE_MS))).await;
            let cleared = box_state_within(&bot.kind, &read().await?, typed_tail(text)) == BoxState::Empty;
            return Ok(Typing::Done(if cleared {
                Delivered::NotAttempted { reason: "paste_truncated", retry: true }
            } else {
                Delivered::Unproven("paste_truncated_uncleared")
            }));
        }
        BoxState::NonEmpty => {}
        BoxState::Empty if proof != Proof::Unverified && evidence(&bot.kind, &proof, offset, &seen, text)? > baseline => {
            return Ok(Typing::Done(Delivered::Submitted));
        }
        BoxState::Empty => return Ok(Typing::Done(Delivered::Unproven("nothing_typed"))),
        BoxState::Unready => return Ok(Typing::Done(Delivered::Unproven("composer_unreadable"))),
    }
    Ok(Typing::Ready(Typed { pane, proof, submit, offset, baseline, structured }))
}

/// 按送出鍵。插隊送出時，會打斷正在跑的那一回合的是**這一顆鍵**，不是上面打的字——claude 忙的時候框裡照樣可以打字，
/// 回合照跑（#120）。
pub async fn press_submit(client: &HerdrClient, t: &Typed) -> anyhow::Result<()> {
    client.pane_send_keys(&t.pane, t.submit.keys()).await
}

fn may_press_retry_submit(
    kind: &str,
    screen: &str,
    text: &str,
    positive_evidence: bool,
    pressed_again: bool,
) -> bool {
    !pressed_again
        && !positive_evidence
        && box_state_within(kind, screen, typed_tail(text)) == BoxState::NonEmpty
        && !super::pane_text::invisible_review_notice(screen)
        && composer_still_holds(kind, screen, text)
}

/// 框裡還是**這一則**草稿：畫面讀回的列逐列對得上原文。列**內**的字與空白要跟原文逐字相同；只有列與列之間的接縫可以是
/// 原文的「沒有字元」（中文或長字被硬折）、「一段空白」（在空白處折行）、「換行」（原文本來就換行）。
/// 以前整個把空白拿掉再比（#1598），`send to ann a` 與 `send to anna` 也算同一則，補按的 Enter 把使用者剛改的字送出去（#1666）。
fn composer_still_holds(kind: &str, screen: &str, text: &str) -> bool {
    super::poller::box_shows_our_folded_paste(kind, screen, text)
        || crate::composer_parse::composer_text_whole_within(kind, screen, typed_tail(text)).is_some_and(|shown| rows_spell_text(&shown, text))
}

/// `shown` 是 `composer_text_whole` 以 `\n` 接起來的列。空列要對上原文的空行（一個空列＝多一個換行），
/// 接縫沒有任何字元時畫面不能多出縮排（原文沒有空白，那是加進去的）。
pub(super) fn rows_spell_text(shown: &str, text: &str) -> bool {
    let mut rest = text.trim();
    let mut blank_rows = 0;
    for row in shown.split('\n') {
        let body = row.trim_start();
        if body.is_empty() {
            blank_rows += 1;
            continue;
        }
        let after_seam = rest.trim_start();
        let seam = &rest[..rest.len() - after_seam.len()];
        let newlines = seam.matches('\n').count();
        let lines_ok = if blank_rows > 0 { newlines == blank_rows + 1 } else { newlines <= 1 };
        if !lines_ok || (seam.is_empty() && body.len() < row.len()) {
            return false;
        }
        let Some(after_row) = after_seam.strip_prefix(body) else { return false };
        rest = after_row;
        blank_rows = 0;
    }
    rest.is_empty()
}

/// 去掉所有空白字元，只用來比「是不是同一串字」。
fn bare(s: &str) -> String {
    s.chars().filter(|c| !c.is_whitespace()).collect()
}

/// 框裡（去掉空白後）還找得到本則原文，只是後面可能接了別的字。claude 送出前會清掉隱形字元，所以比的是清過的樣子。
fn text_still_in_box(kind: &str, screen: &str, text: &str) -> bool {
    if super::poller::box_shows_our_folded_paste(kind, screen, text) {
        return true;
    }
    let needle = bare(&super::pane_text::for_pane(kind, text));
    crate::composer_parse::composer_text_whole_within(kind, screen, typed_tail(text)).is_some_and(|shown| bare(&shown).contains(&needle))
}

/// 送出鍵按下去之後：先看送達證據，完整原文仍留在框裡才最多補按一次。
pub async fn confirm_submitted(
    app: &(impl crate::capabilities::Db + crate::lifecycle::send_now::ports::CodexSendPort
        + crate::capabilities::HerdrRoutes + crate::hosts::HostsAccess),
    client: &HerdrClient,
    run: &db::Run,
    bot: &db::Bot,
    text: &str,
    t: &Typed,
) -> anyhow::Result<Delivered> {
    let Typed { pane, proof, submit, offset, baseline, structured } = t;
    let (offset, baseline) = (*offset, *baseline);
    let read = || read_composer(client, pane);
    super::late_proof::clear(&run.id);
    let submitted_at = db::now();
    let mut pressed_again = false;
    let mut history_conn = None;
    for _ in 0..SUBMIT_CHECKS {
        tokio::time::sleep(Duration::from_millis(SUBMIT_SETTLE_MS)).await;
        let now = read().await?;
        if bot.kind == "claude" && crate::tui_prompts::cold_resume_choice(&now).is_some() {
            if super::claude_resume_menu::accept_if_shown(app, run).await.is_err() {
                return Ok(Delivered::Unproven(super::claude_resume_menu::PENDING));
            }
            continue;
        }
        match same_session(app, &run.id, proof).await {
            SessionMatch::Same => {}
            SessionMatch::Changed => {
                tracing::warn!(run = %run.id, bot = %bot.name, "the session changed while delivering; the transcript proof no longer applies");
                return Ok(Delivered::Unproven("session_changed"));
            }
            SessionMatch::Unavailable => continue,
        }
        let state = box_state_within(&bot.kind, &now, typed_tail(text));
        let proof_evidence = evidence(&bot.kind, proof, offset, &now, text).ok();
        if !proof_evidence.is_some_and(|n| n > baseline) {
            if let Some(mark) = structured {
                if app.codex_prompt_landed(mark, &mut history_conn, text).await {
                    tracing::info!(run = %run.id, bot = %bot.name, "prompt proven submitted by the codex thread history");
                    crate::codex_history::note("delivery", crate::codex_history::Evidence::Structured);
                    return Ok(Delivered::Submitted);
                }
            }
        }
        if proof_evidence.is_some_and(|n| n > baseline) {
            tracing::info!(run = %run.id, bot = %bot.name, proof = ?proof, "prompt typed into the pane and proven submitted");
            note_codex_delivery(&bot.kind, if matches!(proof, Proof::Transcript { .. }) { crate::codex_history::Evidence::Rollout } else { crate::codex_history::Evidence::Screen });
            return Ok(Delivered::Submitted);
        }
        match state {
            BoxState::Empty if *proof == Proof::Unverified => {
                tracing::warn!(run = %run.id, bot = %bot.name, "prompt typed and submitted; no lossless evidence on this run");
                note_codex_delivery(&bot.kind, crate::codex_history::Evidence::Screen);
                return Ok(Delivered::Unverified);
            }
            BoxState::NonEmpty if super::pane_text::invisible_review_notice(&now) => {
                tracing::warn!(run = %run.id, bot = %bot.name, "claude is asking to review a prompt it cleaned of invisible characters; not pressing Enter");
                return Ok(Delivered::Unproven("invisible_chars_review"));
            }
            BoxState::NonEmpty if !composer_still_holds(&bot.kind, &now, text) => {
                super::unsent_draft::diagnose_box_once(&run.id, &bot.kind, &now, text, BoxState::NonEmpty);
            }
            BoxState::Unready => {}
            BoxState::NonEmpty if may_press_retry_submit(&bot.kind, &now, text, false, pressed_again) => {
                tracing::warn!(run = %run.id, bot = %bot.name, ?submit, retry = 1, max_retries = 1, "prompt still in the composer after the submit key; pressing it again");
                client.pane_send_keys(pane, submit.keys()).await?;
                pressed_again = true;
            }
            _ => {}
        }
    }
    let last = read().await?;
    let state = box_state_within(&bot.kind, &last, typed_tail(text));
    if state == BoxState::Empty {
        if let Some(mark) = structured {
            if app.codex_prompt_landed(mark, &mut history_conn, text).await {
                tracing::info!(run = %run.id, bot = %bot.name, "prompt proven submitted by the codex thread history");
                crate::codex_history::note("delivery", crate::codex_history::Evidence::Structured);
                return Ok(Delivered::Submitted);
            }
        }
    }
    let why = match state {
        BoxState::NonEmpty if composer_still_holds(&bot.kind, &last, text) => "still_in_box",
        BoxState::Unready => "composer_unreadable",
        BoxState::NonEmpty => "composer_changed_or_unreadable",
        BoxState::Empty => "not_proven_submitted",
    };
    let text_in_box = state == BoxState::NonEmpty && text_still_in_box(&bot.kind, &last, text);
    if !text_in_box {
        let pending = super::late_proof::Pending::new(&bot.kind, pane, proof, offset, baseline, text, submitted_at);
        super::late_proof::note(&run.id, pending.clone());
        if let Err(e) = super::late_proof::persist_pending(app.db(), &run.id, &pending).await {
            tracing::warn!(run = %run.id, bot = %bot.name, error = %e, "could not persist late proof baseline; the in-process watcher remains active");
        }
    }
    tracing::warn!(run = %run.id, bot = %bot.name, reason = why, "could not prove the prompt was submitted");
    Ok(Delivered::Unproven(why))
}

/// codex 的送達證據來源計數（#749）：非 codex 不記。
fn note_codex_delivery(kind: &str, source: crate::codex_history::Evidence) {
    if kind == "codex" {
        crate::codex_history::note("delivery", source);
    }
}

/// 送出鍵的 RPC 沒有回、不知道 herdr 按了沒有時，看它到底生效了沒有。
#[derive(Debug)]
pub enum Landed {
    /// 證據說送出去了。
    Yes(Delivered),
    /// 字還整個留在框裡：鍵沒生效。
    No,
    /// 看不出來（框空了但沒有證據、讀不到）。
    Unknown,
}

/// [`Landed`]：只看、**不按任何鍵**。transcript 是無損證據，出現這一則就是送出去了，不必看畫面。
pub async fn submit_landed(app: &impl crate::capabilities::Db, client: &HerdrClient, run: &db::Run, bot: &db::Bot, text: &str, t: &Typed) -> Landed {
    for _ in 0..SUBMIT_CHECKS {
        tokio::time::sleep(Duration::from_millis(SUBMIT_SETTLE_MS)).await;
        if matches!(t.proof, Proof::Transcript { .. })
            && same_session(app, &run.id, &t.proof).await == SessionMatch::Same
            && evidence(&bot.kind, &t.proof, t.offset, "", text).is_ok_and(|n| n > t.baseline)
        {
            return Landed::Yes(Delivered::Submitted);
        }
        let Ok(now) = read_composer(client, &t.pane).await else { continue };
        match box_state_within(&bot.kind, &now, typed_tail(text)) {
            BoxState::NonEmpty => return Landed::No,
            BoxState::Empty if t.proof == Proof::Unverified => return Landed::Yes(Delivered::Unverified),
            BoxState::Empty if t.proof == Proof::EchoRow && evidence(&bot.kind, &t.proof, t.offset, &now, text).is_ok_and(|n| n > t.baseline) => {
                return Landed::Yes(Delivered::Submitted);
            }
            _ => {}
        }
    }
    Landed::Unknown
}

/// 之後還能拿來證明「這一則送出去了」的無損證據：本機 claude／codex 的 session log 在基準之後出現這一則。
/// 只有 transcript 那種證據事後還讀得到；畫面上的回音一捲就沒了。
#[derive(Debug, Clone)]
pub struct SentProof {
    format: LogFormat,
    path: std::path::PathBuf,
    offset: u64,
    baseline: usize,
    text: String,
}

impl SentProof {
    pub fn of(t: &Typed, text: &str) -> Option<SentProof> {
        match &t.proof {
            Proof::Transcript { format, path, .. } => {
                Some(SentProof { format: *format, path: path.clone(), offset: t.offset, baseline: t.baseline, text: text.to_string() })
            }
            Proof::EchoRow | Proof::Unverified => None,
        }
    }

    /// 現在看得到這一則了嗎？讀不到當成看不到。
    pub fn shows(&self) -> bool {
        log_hits_since(self.format, &self.path, self.offset, &self.text).is_ok_and(|n| n > self.baseline)
    }
}

/// Plan and execute in one go, for callers that have no turn to hold back (queue flush, resend).
pub async fn deliver_prompt(
    app: &impl LcHost,
    client: &super::RunClient,
    run: &db::Run,
    bot: &db::Bot,
    text: &str,
    force_pane: bool,
    waited_for_log: bool,
) -> anyhow::Result<Delivered> {
    match plan_delivery(app, client, run, bot, text, force_pane, waited_for_log).await? {
        Ok(plan) => execute_delivery(app, client, run, bot, text, plan).await,
        Err(not) => Ok(not),
    }
}

#[cfg(all(test, not(feature = "daemon-test-harness")))]
mod plain_warn_tests {
    use super::*;
    #[test]
    fn the_plain_for_ansi_warning_is_rate_limited_per_pane_and_forgets_old_panes() {
        let t0 = std::time::Instant::now();
        let mut seen = std::collections::HashMap::new();
        assert!(warn_due(&mut seen, "w1:p1", t0));
        assert!(!warn_due(&mut seen, "w1:p1", t0 + std::time::Duration::from_secs(60)), "窗口內不再講");
        assert!(warn_due(&mut seen, "w1:p2", t0 + std::time::Duration::from_secs(60)));
        assert!(warn_due(&mut seen, "w1:p3", t0 + std::time::Duration::from_secs(PLAIN_FOR_ANSI_WARN_SECS + 1)));
        assert!(!seen.contains_key("w1:p1"), "p1 過期被清：{seen:?}");
        assert!(seen.contains_key("w1:p2") && seen.contains_key("w1:p3"), "窗口內的留著：{seen:?}");
    }
}

#[cfg(all(test, not(feature = "daemon-test-harness")))]
mod tests {
    /// 送後核對的補按規則（`may_press_retry_submit`）：只有「完整比對仍是自己的草稿、沒有送達證據、還沒補按過」才補一次。
    const LONG_SCREEN: &str = include_str!("fixtures/codex-synthetic-long-draft.txt");
    const LONG_TEXT: &str = include_str!("fixtures/codex-synthetic-long-draft.expected.txt");

    #[test]
    fn a_long_codex_draft_gets_exactly_one_retry_press_when_the_first_enter_did_not_take() {
        let text = LONG_TEXT.trim_end_matches('\n');
        assert!(super::may_press_retry_submit("codex", LONG_SCREEN, text, false, false), "第一次 Enter 沒生效：補按一次");
        assert!(!super::may_press_retry_submit("codex", LONG_SCREEN, text, false, true), "已經補按過就不再按");
        assert!(!super::may_press_retry_submit("codex", LONG_SCREEN, text, true, false), "已有送達證據不按");
    }

    #[test]
    fn a_history_prompt_or_a_truncated_or_menu_frame_never_gets_a_retry_press() {
        let text = LONG_TEXT.trim_end_matches('\n');
        assert!(!super::may_press_retry_submit("codex", "› 舊問題\n• 舊回覆\n", "舊問題", false, false));
        let truncated: String = LONG_SCREEN.lines().take(12).collect::<Vec<_>>().join("\n");
        assert!(!super::may_press_retry_submit("codex", &truncated, text, false, false), "截斷不補按");
        let review = include_str!("fixtures/codex-0.169.1-hooks-review.txt");
        assert!(!super::may_press_retry_submit("codex", review, text, false, false), "選單不補按");
    }

    #[test]
    fn macos_local_a_numbered_choice_is_not_a_composer_draft() {
        let menu = "Choose an option\n❯ 1. Example first\n  2. Example second\n";
        assert_eq!(super::box_state("claude", menu), super::BoxState::Unready);
        assert_eq!(super::box_state_within("claude", menu, 24), super::BoxState::Unready);
        let ansi = "Choose an option\n\x1b[35m❯ 1. Example first\x1b[0m\n  2. Example second\n";
        assert_eq!(super::box_state("claude", ansi), super::BoxState::Unready);
        assert_eq!(super::box_state("agy", menu), crate::composer_parse::box_state("agy", menu));
    }
    #[test]
    fn macos_local_a_numbered_draft_or_an_old_menu_above_a_composer_is_not_an_open_choice() {
        let draft = "────────────\n❯ 1. First item\n  2. Second item\n────────────\n";
        assert!(!super::claude_open_choice_menu(draft));
        assert_eq!(super::box_state("claude", draft), super::BoxState::NonEmpty);
        let quoted = "Earlier answer\n❯ 1. First option\n  2. Second option\n────────────\n❯\n────────────\n";
        assert!(!super::claude_open_choice_menu(quoted));
        assert_eq!(super::box_state("claude", quoted), super::BoxState::Empty);
    }
    /// 送達掃描用的 source 必須是 herdr 認得的（純斷言，任何環境都跑）。真的 herdr 會不會收，另有
    /// `the_scan_source_is_one_real_herdr_accepts`（只有明講才連）。
    #[test]
    fn the_scan_source_is_a_known_herdr_source() {
        assert!(["visible", "recent", "recent_unwrapped", "detection"].contains(&SCAN_SOURCE));
    }
    /// 要不要連真的 herdr：只有 `AM_TEST_REAL_HERDR=1` 才連。pane 裡本來就有 `HERDR_*`，不能當成同意（#1147）。
    fn real_herdr_target(opt_in: Option<&str>, sock: Option<String>, pane: Option<String>) -> Option<(String, String)> {
        if opt_in != Some("1") {
            return None;
        }
        Some((sock?, pane?))
    }
    #[test]
    fn the_real_herdr_probe_is_opt_in() {
        let s = || Some("/s".to_string());
        let p = || Some("w1:p1".to_string());
        assert!(real_herdr_target(None, s(), p()).is_none(), "pane 裡有 HERDR_* 也不連");
        assert!(real_herdr_target(Some("0"), s(), p()).is_none());
        assert!(real_herdr_target(Some("1"), None, p()).is_none());
        assert_eq!(real_herdr_target(Some("1"), s(), p()), Some(("/s".into(), "w1:p1".into())));
    }
    use super::*;
    const RULE: &str = "─────────────────────────────────────────────";
    /// 沒有無損證據時不准重貼：那個判斷只有在證據看得見時才有意義，
    /// 對 `Unverified` 會退化成「框看起來空的就再貼一次」＝貼兩次。
    #[test]
    fn a_proof_we_cannot_read_never_earns_a_second_paste() {
        for (box_empty, grew) in [(true, false), (true, true), (false, false), (false, true)] {
            assert!(!should_repaste(&Proof::Unverified, box_empty, grew), "unverified 不該重貼 {box_empty} {grew}");
        }
        let readable = Proof::EchoRow;
        assert!(should_repaste(&readable, true, false), "框空、證據沒長＝第一次沒進去，要重貼");
        assert!(!should_repaste(&readable, true, true), "證據長了＝進去了，只是框已經被送掉");
        assert!(!should_repaste(&readable, false, false), "框裡有東西＝貼進去了");
    }
    #[test]
    fn replacing_the_composer_after_the_first_enter_never_sends_a_second_enter() {
        let before_first_enter = screen(&[], &["A"]);
        assert_eq!(crate::composer_parse::composer_text_whole("claude", &before_first_enter).as_deref(), Some("A"));

        let after_first_enter = screen(&[], &["B"]);
        let mut enter_count = 1;
        if may_press_retry_submit("claude", &after_first_enter, "A", false, false) {
            enter_count += 1;
        }
        assert_eq!(enter_count, 1, "B was not part of this delivery and must not be submitted");
    }
    /// claude 的摺疊貼上照舊：行數對得上才算還在框裡（回歸）。
    #[test]
    fn a_claude_fold_still_counts_by_its_line_count() {
        let folded = screen(&[], &["[Pasted text #1 +2 lines]"]);
        assert!(composer_still_holds("claude", &folded, "a\nb\nc"));
        assert!(!composer_still_holds("claude", &folded, "a\nb\nc\nd"), "行數不符");
    }
    #[test]
    fn retry_submit_requires_the_whole_original_draft_and_no_positive_evidence() {
        let original = screen(&[], &["first", "last"]);
        assert!(may_press_retry_submit("claude", &original, "first\nlast", false, false));
        assert!(!may_press_retry_submit("claude", &original, "first\nlast", true, false));
        assert!(!may_press_retry_submit("claude", &original, "first\nlast", false, true));
        let changed = screen(&[], &["changed", "last"]);
        assert!(!may_press_retry_submit("claude", &changed, "first\nlast", false, false));
        assert!(!may_press_retry_submit("claude", "unreadable", "first\nlast", false, false));
    }
    /// #1486 的守門不能拿畫面的換行當原文：TUI 把一則長句折成多列（續行縮排兩格），框裡還是這一則，補按 Enter 照常允許。
    /// 字少了、多了、換掉仍然不按（上一個測試）。
    #[test]
    fn a_draft_the_tui_wrapped_across_rows_still_gets_its_retry_enter() {
        let text = "alpha beta gamma delta epsilon";
        let wrapped = screen(&[], &["alpha beta", "gamma delta epsilon"]);
        assert!(may_press_retry_submit("claude", &wrapped, text, false, false), "框裡還是同一則，只是折了行");
        let other = screen(&[], &["alpha beta", "gamma delta other"]);
        assert!(!may_press_retry_submit("claude", &other, text, false, false), "尾段換掉＝別人的草稿");
    }
    /// 中文草稿沒有空白，TUI 折行時換列前後沒有任何字元。`composer_text_whole` 回的是 `"…草稿\n…"`，
    /// 收合空白會把那個換行變成一個空格，跟原文對不上，補按 Enter 就被擋掉（#1571 只修了有空白的情況）。
    #[test]
    fn a_cjk_draft_the_tui_wrapped_without_spaces_still_gets_its_retry_enter() {
        let text = "這是一則很長的中文草稿沒有空白所以會折行";
        let wrapped = screen(&[], &["這是一則很長的中文草稿", "沒有空白所以會折行"]);
        assert!(may_press_retry_submit("claude", &wrapped, text, false, false), "框裡還是同一則，中文折行不會多出空白");
        let other = screen(&[], &["這是一則很長的中文草稿", "沒有空白所以換掉了"]);
        assert!(!may_press_retry_submit("claude", &other, text, false, false), "尾段換掉＝別人的草稿");
    }
    /// issue #1666：#1598 為了中文折行把所有空白刪掉再比，行內的詞界也跟著不見——`send to ann a` 與 `send to anna`、
    /// `處理 a b` 與 `處理 ab` 被當成同一則，使用者剛改的字被補按的 Enter 送出去。列**內**的字與空白要逐字相同。
    #[test]
    fn issue_1666_a_whitespace_only_edit_inside_a_row_never_earns_a_retry_enter() {
        for (text, shown) in [
            ("send to ann a", "send to anna"),
            ("send to anna", "send to ann a"),
            ("處理 a b", "處理 ab"),
            ("處理 ab", "處理 a b"),
            ("alpha beta", "alpha  beta"),
            ("alpha  beta", "alpha beta"),
        ] {
            let box_screen = screen(&[], &[shown]);
            assert!(!may_press_retry_submit("claude", &box_screen, text, false, false), "{text:?} 被改成 {shown:?}：別人的草稿，不補按");
            let untouched = screen(&[], &[text]);
            assert!(may_press_retry_submit("claude", &untouched, text, false, false), "{text:?} 原封不動：補按");
        }
    }
    /// issue #1666：只有列與列之間的接縫可以對上原文的「沒有字元」（硬折）、「空白」（在空白處折）或「換行」（原文本來就換行）；
    /// 原文自己的換行、縮排、空行逐列對得上就是同一則。
    #[test]
    fn issue_1666_only_the_seams_between_rows_may_stand_for_a_missing_wrap() {
        let multi = "第一行\n    縮排的程式\n\n結尾";
        let same = screen(&[], &["第一行", "    縮排的程式", "", "結尾"]);
        assert!(may_press_retry_submit("claude", &same, multi, false, false), "原文的換行、縮排、空行都在");
        let no_blank = screen(&[], &["第一行", "    縮排的程式", "結尾"]);
        assert!(!may_press_retry_submit("claude", &no_blank, multi, false, false), "空行被拿掉＝改過");
        let extra_blank = screen(&[], &["第一行", "", "    縮排的程式", "", "結尾"]);
        assert!(!may_press_retry_submit("claude", &extra_blank, multi, false, false), "多了一個空行＝改過");
        let flat = screen(&[], &["第一行", "", "結尾"]);
        assert!(!may_press_retry_submit("claude", &flat, "第一行\n結尾", false, false), "原文沒有空行，畫面多了一列空的");

        let hard = screen(&[], &["abcdefghij", "klmnop"]);
        assert!(may_press_retry_submit("claude", &hard, "abcdefghijklmnop", false, false), "長字被硬折：接縫沒有字元");
        let soft = screen(&[], &["abcde", "fghij"]);
        assert!(may_press_retry_submit("claude", &soft, "abcde fghij", false, false), "在空白處折：接縫是一段空白");
        assert!(may_press_retry_submit("claude", &soft, "abcde\nfghij", false, false), "原文本來就換行");
        let padded = screen(&[], &["abcde", " fghij"]);
        assert!(!may_press_retry_submit("claude", &padded, "abcdefghij", false, false), "原文接縫沒有空白，畫面卻多出縮排＝加了字");
        let trailing = screen(&[], &["abcde", "fghij", "k"]);
        assert!(!may_press_retry_submit("claude", &trailing, "abcde fghij", false, false), "後面多了字");
        let short = screen(&[], &["abcde"]);
        assert!(!may_press_retry_submit("claude", &short, "abcde fghij", false, false), "少了字");
    }
    #[test]
    fn codex_long_multiline_draft_keeps_blank_rows_for_the_bounded_submit_retry() {
        let rows: Vec<String> = (0..48).map(|i| if i == 17 { String::new() } else { format!("dispatch step {i}: preserve exact text") }).collect();
        let text = rows.join("\n");
        let mut screen = "old output\n".repeat(340);
        screen.push_str(&format!("\u{1b}[36m›\u{1b}[0m {}\n", rows[0]));
        for row in &rows[1..] { screen.push_str(&format!("  {row}\n")); }
        screen.push_str("\ngpt-6.1-sol · context 13%\n");
        assert!(super::may_press_retry_submit("codex", &screen, &text, false, false));
        assert!(!super::may_press_retry_submit("codex", &screen, &text, false, true));
        assert!(!super::may_press_retry_submit("codex", &screen, &text, true, false));
        let changed = screen.replace("dispatch step 47:", "modified step 47:");
        assert!(!super::may_press_retry_submit("codex", &changed, &text, false, false));
        assert!(!super::may_press_retry_submit("codex", &screen.replace('›', " "), &text, false, false));
    }
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
    const CODEX_PARTICLES_EMPTY: &str = include_str!("fixtures/codex_astra_particles_empty.ansi");
    const CODEX_PARTICLES_DRAFT: &str = include_str!("fixtures/codex_astra_particles_draft.ansi");
    const CODEX_0157_RESTORED_ANSWER: &str = include_str!("fixtures/codex-0.157-question-turn-end-restored.txt");
    /// Upstream Codex #47422's turn-end snapshot: existing composer text followed by recovered,
    /// unsent question answers. This remains a draft from the daemon's point of view.
    #[test]
    fn codex_0157_question_turn_end_snapshot_is_a_nonempty_composer() {
        assert_eq!(box_state("codex", CODEX_0157_RESTORED_ANSWER), BoxState::NonEmpty);
    }
    /// codex v0.154.0（gpt-6-astra）輸入列的點字動畫：2026-09-15 在 herdr pane 開一個 codex、`pane read --format ansi`
    /// 實抓（fixtures/codex_astra_particles_*.ansi）。上下兩列與 marker 列都撒了帶顏色、非 dim 的 ⠁⠂⠄⠈⠐⠠⢀。
    #[test]
    fn codex_braille_particles_are_not_a_draft_on_a_styled_read() {
        assert_eq!(box_state("codex", CODEX_PARTICLES_EMPTY), BoxState::Empty, "只有 dim 佔位字與點字底紋");
        let glued = CODEX_PARTICLES_EMPTY.replacen(
            "\u{1b}[1m\u{1b}[48;2;59;64;76m›\u{1b}[0m\u{1b}[48;2;59;64;76m \u{1b}[0m",
            "\u{1b}[1m\u{1b}[48;2;59;64;76m›\u{1b}[0m\u{1b}[38;2;85;89;99m\u{1b}[48;2;59;64;76m⠁\u{1b}[0m",
            1,
        );
        assert_ne!(glued, CODEX_PARTICLES_EMPTY, "fixture 的 marker 形狀沒變");
        assert_eq!(box_state("codex", &glued), BoxState::Empty, "點字蓋掉 marker 後的空格");
        assert_eq!(box_state("codex", CODEX_PARTICLES_DRAFT), BoxState::NonEmpty, "點字底紋＋真草稿");
    }
    /// 2026-09-23 真畫面（claude 2.1.28x，`/model claude-opus-5-5` 套用後閒置）：輸入列是 `❯ ` 加一串普通空格——
    /// 終端留下的 padding。之前被當成草稿回 composer_busy，使用者從網頁送不進字。
    #[test]
    fn a_claude_composer_padded_with_blank_cells_after_a_model_switch_is_empty() {
        const SCREEN: &str = include_str!("fixtures/claude-2.1.28x-composer-padding-after-model.ansi");
        assert_eq!(box_state("claude", SCREEN), BoxState::Empty);
        let typed = SCREEN.replacen("❯\u{a0}   ", "❯\u{a0}x  ", 1);
        assert_ne!(typed, SCREEN);
        assert_eq!(box_state("claude", &typed), BoxState::NonEmpty);
    }
    #[test]
    fn braille_is_only_ignored_when_it_is_a_coloured_codex_particle() {
        let plain: String = CODEX_PARTICLES_EMPTY.lines().map(strip_ansi).collect::<Vec<_>>().join("\n");
        assert_eq!(box_state("codex", &plain), BoxState::NonEmpty, "plain read");
        let row = |marker_row: &str, below: &str| format!("\u{1b}[0m\n{marker_row}\n{below}\n\n  gpt-6-astra medium · ~/proj\n");
        let dim_ph = "\u{1b}[1m›\u{1b}[0m \u{1b}[2mAsk Codex to do anything\u{1b}[0m";
        let particle = "\u{1b}[38;2;139;141;148m⠐\u{1b}[0m";
        assert_eq!(box_state("codex", &row(&format!("\u{1b}[1m›\u{1b}[0m ⠁⠂"), "")), BoxState::NonEmpty, "uncoloured braille typed");
        assert_eq!(box_state("codex", &row(&format!("{dim_ph}   {particle}  x"), "")), BoxState::NonEmpty, "one ordinary char");
        assert_eq!(box_state("codex", &row(&format!("{dim_ph}  {particle}"), &format!("      {particle}   {particle}"))), BoxState::Empty);
        assert_eq!(box_state("codex", &row(&format!("{dim_ph}  {particle}"), &format!("  more {particle}"))), BoxState::NonEmpty, "second line typed");
        let claude = format!("─────\n❯ {particle}\n─────\n");
        assert_eq!(box_state("claude", &claude), BoxState::NonEmpty);
    }
    /// grok 的建議句是**暗灰前景**（`38;2;88;88;88`），不是 SGR 2 的 dim：只看 dim 會把它當成
    /// 使用者打的草稿，那顆 bot 從此每一則 prompt 都 409 `composer_busy`（2026-09-19 w168:p7J，
    /// 使用者打了字送不出去，畫面上框裡只有一句灰色的「繼續寫完」）。
    #[test]
    fn a_grok_ghost_suggestion_is_a_hint_not_a_draft() {
        let esc = "\u{1b}";
        let bg = format!("{esc}[48;2;20;20;20m");
        let row = |text_fg: &str| {
            format!(
                "{bg}{esc}[38;2;80;80;88m│ {esc}[38;2;200;200;200m❯ {esc}[38;2;{text_fg}m繼續寫完{bg}          {esc}[38;2;80;80;88m│"
            )
        };
        let screen = format!("⏺ 先前的輸出\n{}\n╰──────────────╯\n", row("88;88;88"));
        assert_eq!(box_state("grok", &screen), BoxState::Empty, "灰色建議不是草稿");

        let screen = format!("⏺ 先前的輸出\n{}\n╰──────────────╯\n", row("225;225;225"));
        assert_eq!(box_state("grok", &screen), BoxState::NonEmpty, "亮色是使用者打的字");
    }
    /// 顏色比的是**跟 marker 的對比**，不是寫死的門檻：淺色主題（白底黑字）一樣分得出來。
    #[test]
    fn the_hint_rule_works_on_a_light_theme_too() {
        let marker = Cell { ch: '❯', dim: false, fg: true, fg_rgb: Some((20, 20, 20)), bg_rgb: Some((250, 250, 250)) };
        let hint = Cell { ch: '字', dim: false, fg: true, fg_rgb: Some((190, 190, 190)), bg_rgb: Some((250, 250, 250)) };
        let typed = Cell { ch: '字', dim: false, fg: true, fg_rgb: Some((30, 30, 30)), bg_rgb: Some((250, 250, 250)) };
        assert!(is_hint_cell(&hint, Some(&marker)), "淺色主題的灰提示");
        assert!(!is_hint_cell(&typed, Some(&marker)), "淺色主題打的字");
        assert!(is_hint_cell(&Cell { ch: 'x', dim: true, fg: false, fg_rgb: None, bg_rgb: None }, None));
        assert!(!is_hint_cell(&Cell { ch: 'x', dim: false, fg: false, fg_rgb: None, bg_rgb: None }, Some(&marker)));
    }
    #[test]
    fn styled_cells_track_the_foreground_colour() {
        let got = styled_cells("a\u{1b}[38;5;2mb\u{1b}[39mc\u{1b}[31md\u{1b}[0me\u{1b}[48;2;1;2;3mf");
        let fg: Vec<(char, bool)> = got.iter().map(|c| (c.ch, c.fg)).collect();
        assert_eq!(fg, vec![('a', false), ('b', true), ('c', false), ('d', true), ('e', false), ('f', false)]);
    }
    const CLAUDE_280_SUGGESTION: &str = include_str!("fixtures/claude-2.1.280-prompt-suggestion.ansi");
    /// Claude Code 2.1.280 的「建議下一句」（prompt suggestion）：`❯` ＋ NBSP ＋ `\x1b[0m` ＋ `\x1b[2m…\x1b[0m`。
    /// 真畫面（2026-09-23 在獨立 herdr session 裡開 claude 抓的 `recent-unwrapped` ansi 讀）要讀成空框；
    /// 2026-09-23 AM-2-M 就是被這一行擋成 409 `composer_busy`（「bot 的輸入框裡有字」）。
    #[test]
    fn claude_280_prompt_suggestion_is_an_empty_box() {
        assert!(CLAUDE_280_SUGGESTION.contains("❯\u{a0}\u{1b}[0m\u{1b}[2mInitialize git\u{1b}[0m"), "fixture 形狀變了");
        assert_eq!(box_state("claude", CLAUDE_280_SUGGESTION), BoxState::Empty, "真畫面的建議句");
        let agm = CLAUDE_280_SUGGESTION.replace("Initialize git", "推給agm強制部署");
        assert_eq!(box_state("claude", &agm), BoxState::Empty, "AM-2-M 那一句");
        let typed = CLAUDE_280_SUGGESTION.replace("\u{1b}[2mInitialize git", "Initialize git");
        assert_eq!(box_state("claude", &typed), BoxState::NonEmpty, "非 dim＝打的字");
        let mixed = CLAUDE_280_SUGGESTION.replace("Initialize git\u{1b}[0m", "Initialize git\u{1b}[0mx");
        assert_eq!(box_state("claude", &mixed), BoxState::NonEmpty, "同一行混了正常字");
        let mixed_front = CLAUDE_280_SUGGESTION.replace("\u{1b}[2mInitialize", "x\u{1b}[2mInitialize");
        assert_eq!(box_state("claude", &mixed_front), BoxState::NonEmpty, "正常字在前");
    }
    /// 網頁要顯示的「建議下一句」：真畫面（2.1.280）的灰字取得出來，跟 [`box_state`] 的「框是空的」是同一個結論；
    /// 純文字讀、使用者打的字、混了實字的一行、別的 kind 都取不出來（寧可不顯示，不把草稿當建議）。
    #[test]
    fn prompt_suggestion_is_the_dim_text_in_the_empty_box() {
        assert_eq!(prompt_suggestion("claude", CLAUDE_280_SUGGESTION).as_deref(), Some("Initialize git"));
        let cjk = CLAUDE_280_SUGGESTION.replace("Initialize git", "推給agm強制部署");
        assert_eq!(prompt_suggestion("claude", &cjk).as_deref(), Some("推給agm強制部署"));
        let plain: String = CLAUDE_280_SUGGESTION.lines().map(strip_ansi).collect::<Vec<_>>().join("\n");
        assert!(plain.contains("Initialize git"), "前提：純文字裡看得到那幾個字");
        assert_eq!(prompt_suggestion("claude", &plain), None);
        let typed = CLAUDE_280_SUGGESTION.replace("\u{1b}[2mInitialize git", "Initialize git");
        assert_eq!(prompt_suggestion("claude", &typed), None, "打的字不是建議");
        let mixed = CLAUDE_280_SUGGESTION.replace("Initialize git\u{1b}[0m", "Initialize git\u{1b}[0mx");
        assert_eq!(prompt_suggestion("claude", &mixed), None, "同一行混了正常字");
        assert_eq!(prompt_suggestion("codex", CLAUDE_280_SUGGESTION), None);
        assert_eq!(prompt_suggestion("claude", &CLAUDE_280_SUGGESTION.replace("\u{1b}[2mInitialize git\u{1b}[0m", "")), None);
        let long = CLAUDE_280_SUGGESTION.replace("Initialize git", &"長".repeat(SUGGESTION_MAX_CHARS + 1));
        assert_eq!(prompt_suggestion("claude", &long), None);
    }
    /// 窄 pane 把建議折成兩列：續行接在後面；兩側都是 ASCII 補一個空白，中文直接接。
    #[test]
    fn a_wrapped_prompt_suggestion_is_joined_into_one_line() {
        let wrapped = |first: &str, second: &str| {
            CLAUDE_280_SUGGESTION.replace(
                "\u{1b}[2mInitialize git\u{1b}[0m\r\n",
                &format!("\u{1b}[2m{first}\u{1b}[0m\r\n  \u{1b}[2m{second}\u{1b}[0m\r\n"),
            )
        };
        let screen = wrapped("Run the full test", "suite again");
        assert_ne!(screen, CLAUDE_280_SUGGESTION, "fixture 換行的寫法變了，這個測試要跟著改");
        assert_eq!(prompt_suggestion("claude", &screen).as_deref(), Some("Run the full test suite again"));
        assert_eq!(prompt_suggestion("claude", &wrapped("跑一次完整", "測試再收尾")).as_deref(), Some("跑一次完整測試再收尾"));
        let typed_tail = CLAUDE_280_SUGGESTION.replace("\u{1b}[2mInitialize git\u{1b}[0m\r\n", "\u{1b}[2mInitialize git\u{1b}[0m\r\n  typed\r\n");
        assert_eq!(prompt_suggestion("claude", &typed_tail), None);
    }
    /// 讀純文字判斷輸入框的地方（`composer_text`、`tui_prompts::composer_is_idle`）跟 [`box_state`] 同一個結論：
    /// 只有建議句＝空輸入列；真的字、混了一個正常字＝原樣保留。
    #[test]
    fn plain_checks_see_a_suggestion_only_box_as_empty() {
        let plain = plain_without_hints("claude", CLAUDE_280_SUGGESTION);
        assert!(!plain.contains('\u{1b}') && !plain.contains("Initialize git"), "{plain}");
        let lines: Vec<&str> = plain.lines().collect();
        assert!(crate::tui_prompts::composer_is_idle(&lines), "{plain}");
        assert_eq!(crate::lifecycle::poller::composer_text("claude", CLAUDE_280_SUGGESTION), None);

        let typed = CLAUDE_280_SUGGESTION.replace("\u{1b}[2mInitialize git", "Initialize git");
        assert_eq!(crate::lifecycle::poller::composer_text("claude", &typed).as_deref(), Some("Initialize git"));
        let mixed = CLAUDE_280_SUGGESTION.replace("Initialize git\u{1b}[0m", "Initialize git\u{1b}[0mx");
        assert!(plain_without_hints("claude", &mixed).contains("Initialize gitx"), "混了正常字就整句保留");
        let bare = strip_ansi(CLAUDE_280_SUGGESTION);
        assert_eq!(plain_without_hints("claude", &bare), bare);
    }
    /// 建議句在窄 pane 折成兩列：續行一樣只有提示字，整個框還是空的；續行上有一個正常字就是草稿。
    #[test]
    fn a_suggestion_wrapped_onto_a_second_row_is_still_an_empty_box() {
        let two = CLAUDE_280_SUGGESTION.replace(
            "\u{1b}[2mInitialize git\u{1b}[0m",
            "\u{1b}[2mInitialize a git repository and\u{1b}[0m\r\n  \u{1b}[2mcommit notes.txt\u{1b}[0m",
        );
        assert_eq!(box_state("claude", &two), BoxState::Empty);
        assert!(!plain_without_hints("claude", &two).contains("commit notes"));
        let typed_second = two.replace("\u{1b}[2mcommit notes.txt", "commit notes.txt");
        assert_eq!(box_state("claude", &typed_second), BoxState::NonEmpty);
        assert!(plain_without_hints("claude", &typed_second).contains("commit notes.txt"));
    }
    const CLAUDE_2326_FAILED_EMPTY: &str = include_str!("fixtures/claude-2.1.326-failed-empty-hint.ansi");
    const CLAUDE_2326_FAILED_TYPED: &str = include_str!("fixtures/claude-2.1.326-failed-typed.ansi");
    /// 2.1.326 請求失敗（模型不存在）後的空框：`❯ ` 接 dim 的 `Once fixed, resend your message`（實機擷取）。
    /// 樣式讀：dim 的提示＝空框；同一格位置使用者真的打的 `abc`（不是 dim）＝草稿。
    #[test]
    fn claude_2326_failed_request_hint_is_an_empty_box_when_styled() {
        assert_eq!(box_state("claude", CLAUDE_2326_FAILED_EMPTY), BoxState::Empty);
        assert_eq!(box_state("claude", CLAUDE_2326_FAILED_TYPED), BoxState::NonEmpty);
        assert_eq!(crate::lifecycle::poller::composer_text("claude", CLAUDE_2326_FAILED_EMPTY), None);
        assert_eq!(crate::lifecycle::poller::composer_text("claude", CLAUDE_2326_FAILED_TYPED).as_deref(), Some("abc"));
    }
    /// 純文字讀（herdr 不給 ansi）：composer 列內容**完全等於**實機字串才是空框；多一個字一律是草稿。
    /// 含 try／retry／resend 的使用者輸入不得判成空框（沿用 7d4c38a2d 的回歸精神）。
    #[test]
    fn claude_2326_failed_request_hint_is_an_empty_box_when_plain_only_if_exact() {
        let plain = strip_ansi(CLAUDE_2326_FAILED_EMPTY);
        assert_eq!(crate::lifecycle::poller::composer_text("claude", &plain), None, "{plain}");
        let read = plain_without_hints("claude", &plain);
        let lines: Vec<&str> = read.lines().collect();
        assert!(crate::tui_prompts::composer_is_idle(&lines), "{read}");

        const HINT: &str = "Once fixed, resend your message";
        for text in ["try again", "retry", "resend", "Once fixed, resend your message please", "Once fixed, resend"] {
            let typed = plain.replace(HINT, text);
            assert_eq!(crate::lifecycle::poller::composer_text("claude", &typed).as_deref(), Some(text), "{text}");
        }
    }
    /// 只有已知的空框形狀才算空：任何額外位元組、任何續行（含空白列）都是非空（第七輪 #1）。
    #[test]
    fn only_the_exact_empty_shape_is_an_empty_box() {
        let empty = screen(&["⏺ 先前的回覆"], &[]);
        assert_eq!(box_state("claude", &empty), BoxState::Empty);
        for (row, why) in [("❯  ", "marker 後多一個空白"), ("❯   ", "只有空白")] {
            let s = empty.replacen("❯\n", &format!("{row}\n"), 1);
            assert_eq!(box_state("claude", &s), BoxState::Empty, "{why}");
        }
        let typed = empty.replacen("❯\n", "❯ x\n", 1);
        assert_eq!(box_state("claude", &typed), BoxState::NonEmpty, "有字");
        let blank_second = empty.replacen("❯\n", "❯\n  \n", 1);
        assert_eq!(box_state("claude", &blank_second), BoxState::NonEmpty, "空白第二行");
        let second_line = empty.replacen("❯\n", "❯\n  草稿\n", 1);
        assert_eq!(box_state("claude", &second_line), BoxState::NonEmpty, "第二行有字");
        let plain = empty.replacen("❯\n", "❯ Try \"fix lint errors\"\n", 1);
        assert_eq!(box_state("claude", &plain), BoxState::NonEmpty, "純文字的佔位字");
        let dim = empty.replacen("❯\n", "❯ \u{1b}[2mTry \"fix lint errors\"\u{1b}[0m\n", 1);
        assert_eq!(box_state("claude", &dim), BoxState::Empty, "dim 的佔位字");
        let suggestion = empty.replacen("❯\n", "❯\u{a0}\u{1b}[2m把 4b 和 4c 補做完\u{1b}[0m\r\n", 1);
        assert_eq!(box_state("claude", &suggestion), BoxState::Empty, "dim 的建議句");
        let typed_same = empty.replacen("❯\n", "❯\u{a0}把 4b 和 4c 補做完\n", 1);
        assert_eq!(box_state("claude", &typed_same), BoxState::NonEmpty, "非 dim 的同一句");
        let mixed = empty.replacen("❯\n", "❯\u{a0}\u{1b}[2m把 4b 和 4c\u{1b}[0m 補做完\n", 1);
        assert_eq!(box_state("claude", &mixed), BoxState::NonEmpty, "混進非 dim 字");
        let dim_codex = "› \u{1b}[2mSomething codex suggests\u{1b}[0m\n\ngpt-6-astra low · ~/proj\n";
        assert_eq!(box_state("codex", dim_codex), BoxState::Empty, "codex 任何 dim 提示");
        let dim_spaces = empty.replacen("❯\n", "❯ \u{1b}[2m  \u{1b}[0m\n", 1);
        assert_eq!(box_state("claude", &dim_spaces), BoxState::Empty, "只有空白是 padding");
        let plain_spaces = empty.replacen("❯\n", "❯\u{a0}                      \n", 1);
        assert_eq!(box_state("claude", &plain_spaces), BoxState::Empty, "普通空格的 padding");
        let past_placeholder = empty.replacen("❯\n", "❯ Try \"fix lint errors\" now\n", 1);
        assert_eq!(box_state("claude", &past_placeholder), BoxState::NonEmpty);
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
        assert_eq!(echo_row_hits("claude", &screen(&["❯ ab", "  plain reply text"], &[]), "ab"), 0);
    }
    /// issue #1479：claude 的回音列是灰底整列寬，styled 讀取把列尾補成空白。補白不算內容；內容本身仍要逐字相等。
    #[test]
    fn a_padded_echo_row_still_counts_but_only_when_the_text_is_exact() {
        let pad = " ".repeat(140);
        let row = |t: &str| screen(&[&format!("❯ {t}{pad}")], &[]);
        assert_eq!(echo_row_hits("claude", &row("Reply with PONG"), "Reply with PONG"), 1);
        assert_eq!(echo_row_hits("claude", &row("Reply with PON"), "Reply with PONG"), 0, "少一個字");
        assert_eq!(echo_row_hits("claude", &row("Reply with PONG!"), "Reply with PONG"), 0, "多一個字");
        assert_eq!(echo_row_hits("claude", &row(" Reply with PONG"), "Reply with PONG"), 0, "marker 後多一個空白");
        assert_eq!(echo_row_hits("claude", &screen(&[&format!("❯ ab{pad}"), "  cd"], &[]), "ab"), 0, "補白的列底下接縮排文字還是續行");
        let styled = format!("\u{1b}[0m\u{1b}[48;2;55;55;55m❯ \u{1b}[38;2;255;255;255mReply with PONG{pad}\u{1b}[0m\n❯\n");
        assert_eq!(evidence("claude", &Proof::EchoRow, 0, &styled, "Reply with PONG").unwrap(), 1);
    }
    #[test]
    fn a_grok_echo_row_ignores_its_right_aligned_clock_and_scrollbar() {
        let screen = "❯ Reply with GROK-OK                    2:09 AM   █\n\n  ╭──────────────────────────────╮\n  │ ❯                            │\n  ╰──────── Grok 4.6 (low) ──────╯\n";
        assert_eq!(echo_row_hits("grok", screen, "Reply with GROK-OK"), 1);
        assert_eq!(echo_row_hits("grok", screen, "Reply with GROK-NO"), 0);

        let continued = screen.replace("\n\n  ╭", "\n  plain reply text\n\n  ╭");
        assert_eq!(echo_row_hits("grok", &continued, "Reply with GROK-OK"), 0, "續行不應被誤當完整回音");

        let prose_clock = screen.replace("Reply with GROK-OK                    2:09 AM   █", "meet at 2:09 PM");
        assert_eq!(echo_row_hits("grok", &prose_clock, "meet at 2:09 PM"), 1, "正文時間不能被當成裝飾剝掉");
    }
    fn inputs<'a>(kind: &'a str, local: bool, session: Option<&'a str>, path: Option<&'a str>, codex_log: Option<std::path::PathBuf>, cols: Option<u32>) -> ProofInputs<'a> {
        ProofInputs { kind, host_is_local: local, hooks: true, session_id: session, transcript_path: path, codex_log, waited_for_log: false, pane_cols: cols }
    }
    /// 證據矩陣（SPEC §4.4a）：provider × 本機／遠端 × 單行／多行 → 用哪種證據，或 unverified。
    #[test]
    fn the_evidence_matrix() {
        let dir = crate::test_dirs::track(std::env::temp_dir().join(format!("am-proof-{}", db::ulid())));
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
            ("codex 本機 session 已知但 rollout 未寫", inputs("codex", true, Some("s1"), None, None, w), multi,
                Err(Delivered::NotAttempted { reason: "codex_log_not_ready", retry: true })),
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
        let no_hooks = ProofInputs { hooks: false, ..inputs("claude", true, None, None, None, w) };
        assert_eq!(choose_proof(&no_hooks, multi), Ok(Proof::Unverified));
        let huge = "x".repeat(MAX_PROVABLE_CHARS + 1);
        assert_eq!(
            choose_proof(&inputs("claude", true, Some("s1"), Some(tp), None, w), &huge),
            Err(Delivered::NotAttempted { reason: "prompt_too_long_to_prove", retry: false }),
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
    #[test]
    fn the_plain_for_ansi_warning_is_throttled_per_pane() {
        let a = format!("pane-throttle-{}", db::ulid());
        let b = format!("pane-throttle-{}", db::ulid());
        assert!(should_warn_plain_for_ansi(&a));
        assert!(!should_warn_plain_for_ansi(&a), "not again within the window");
        assert!(should_warn_plain_for_ansi(&b), "another pane warns on its own");
    }
    /// SGR 解析：dim 的開關、重設，38/48 顏色參數裡的 `2` 不會被當成 dim。
    #[test]
    fn styled_chars_follow_sgr_exactly() {
        let row = "\u{1b}[48;2;59;64;76ma\u{1b}[2mb\u{1b}[22mc\u{1b}[2;38;5;2md\u{1b}[0me\r";
        let got: Vec<(char, bool)> = styled_chars(row);
        assert_eq!(got, vec![('a', false), ('b', true), ('c', false), ('d', true), ('e', false)]);
        assert_eq!(strip_ansi(row), "abcde");
    }
    /// 同一天有兩個同 session 的 rollout 取最新；超過兩週前的舊 session 也找得到；symlink 指到 sessions 外面不收。
    #[test]
    fn the_codex_rollout_lookup_prefers_the_newest_and_stays_inside_sessions() {
        let home = crate::test_dirs::track(std::env::temp_dir().join(format!("am-codex-lookup-{}", db::ulid())));
        let day = home.join("sessions/2026/09/14");
        let ancient = home.join("sessions/2026/01/02");
        std::fs::create_dir_all(&day).unwrap();
        std::fs::create_dir_all(&ancient).unwrap();
        let older = day.join("rollout-2026-09-14T08-00-00-sess-same.jsonl");
        let newer = day.join("rollout-2026-09-14T09-00-00-sess-same.jsonl");
        std::fs::write(&newer, "").unwrap();
        std::fs::write(&older, "").unwrap();
        let set_mtime = |p: &std::path::Path, secs: u64| {
            std::fs::File::options()
                .write(true)
                .open(p)
                .unwrap()
                .set_modified(std::time::UNIX_EPOCH + std::time::Duration::from_secs(secs))
                .unwrap()
        };
        set_mtime(&newer, 1_800_000_000);
        set_mtime(&older, 1_800_000_060);
        assert_eq!(codex_session_log(&home, "sess-same"), Some(std::fs::canonicalize(&older).unwrap()));
        set_mtime(&newer, 1_800_000_120);
        assert_eq!(codex_session_log(&home, "sess-same"), Some(std::fs::canonicalize(&newer).unwrap()));
        set_mtime(&older, 1_800_000_120);
        assert_eq!(codex_session_log(&home, "sess-same"), Some(std::fs::canonicalize(&newer).unwrap()));
        std::fs::write(ancient.join("rollout-2026-01-02T00-00-00-sess-ancient.jsonl"), "").unwrap();
        for d in 3..=28 {
            std::fs::create_dir_all(home.join(format!("sessions/2026/02/{d:02}"))).unwrap();
        }
        assert!(codex_session_log(&home, "sess-ancient").is_some(), "日期樹整棵走，不限 14 天");
        let outside = home.join("elsewhere.jsonl");
        std::fs::write(&outside, "").unwrap();
        let link = home.join("sessions/2026/09/14/rollout-2026-09-14T10-00-00-sess-link.jsonl");
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(&outside, &link).unwrap();
            assert_eq!(codex_session_log(&home, "sess-link"), None, "指到 sessions 外面的 symlink 不算");
        }
        let _ = std::fs::remove_dir_all(&home);
    }
    /// 日期樹整棵走的查找放到 blocking pool 之後，答案要跟同步版一樣（找得到、找不到都是）。
    #[tokio::test]
    async fn the_async_rollout_lookup_matches_the_sync_one() {
        let home = crate::test_dirs::scratch_dir("am-rollout-async");
        let day = home.join("sessions/2026/09/14");
        std::fs::create_dir_all(&day).unwrap();
        std::fs::write(day.join("rollout-2026-09-14T10-00-00-sess-a.jsonl"), "").unwrap();
        for id in ["sess-a", "sess-missing"] {
            assert_eq!(codex_session_log_async(home.clone(), id.to_string()).await, codex_session_log(&home, id), "{id}");
        }
        assert!(codex_session_log_async(home.clone(), "sess-a".into()).await.is_some());
        assert!(codex_session_log_async(home, "../x".into()).await.is_none(), "不合法的 id 照舊拒絕");
    }
    /// codex session 已知但 rollout 還沒寫出來：先等（可重試），等過了才退回一列回音或 unverified（sol 第九輪 #1）。
    #[test]
    fn a_known_codex_session_waits_for_its_rollout_before_falling_back() {
        let waiting = ProofInputs { waited_for_log: false, ..inputs("codex", true, Some("s1"), None, None, Some(80)) };
        assert_eq!(choose_proof(&waiting, "a\nb"), Err(Delivered::NotAttempted { reason: "codex_log_not_ready", retry: true }));
        assert_eq!(choose_proof(&waiting, "go"), Err(Delivered::NotAttempted { reason: "codex_log_not_ready", retry: true }), "單行也先等無損證據");
        let waited = ProofInputs { waited_for_log: true, ..inputs("codex", true, Some("s1"), None, None, Some(80)) };
        assert_eq!(choose_proof(&waited, "a\nb"), Ok(Proof::Unverified));
        assert_eq!(choose_proof(&waited, "go"), Ok(Proof::EchoRow));
        let unknown = inputs("codex", true, None, None, None, Some(80));
        assert_eq!(choose_proof(&unknown, "a\nb"), Ok(Proof::Unverified));
    }
    /// codex rollout：只算頂層 response_item 的 user 訊息；compacted 重播、非 input_text、developer 都不算；逐字比對。
    #[test]
    fn codex_rollout_user_entries_are_read_exactly() {
        let dir = crate::test_dirs::track(std::env::temp_dir().join(format!("am-codex-{}", db::ulid())));
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
        let home = crate::test_dirs::track(std::env::temp_dir().join(format!("am-codex-home-{}", db::ulid())));
        let old = home.join("sessions/2026/09/01");
        let new = home.join("sessions/2026/09/14");
        std::fs::create_dir_all(&old).unwrap();
        std::fs::create_dir_all(&new).unwrap();
        std::fs::write(old.join("rollout-2026-09-01T00-00-00-01a0aaaa-0000.jsonl"), "").unwrap();
        let want = new.join("rollout-2026-09-14T15-49-57-01a09ee4-eabb-7682-b363-941a606ed002.jsonl");
        std::fs::write(&want, "").unwrap();
        std::fs::write(new.join("rollout-2026-09-14T16-00-00-01a09ee4-eabb-7682-b363-941a606ed0029.jsonl"), "").unwrap();
        let canon = |p: std::path::PathBuf| Some(std::fs::canonicalize(p).unwrap());
        assert_eq!(codex_session_log(&home, "01a09ee4-eabb-7682-b363-941a606ed002"), canon(want));
        assert_eq!(codex_session_log(&home, "01a0aaaa-0000"), canon(old.join("rollout-2026-09-01T00-00-00-01a0aaaa-0000.jsonl")));
        assert_eq!(codex_session_log(&home, "nope"), None);
        assert_eq!(codex_session_log(&home, "../../etc"), None, "不接受會跑出目錄的 id");
        let _ = std::fs::remove_dir_all(&home);
    }
}

#[cfg(all(test, not(feature = "daemon-test-harness")))]
mod api_tests {
    //! Through `prompt()` itself: what reaches the database when nothing could be sent.
    use super::*;
    /// grok 多行沒有無損證據：照樣打字送出，回 200 `unverified`，turn 留下「要人工核對」的標記（不是 unknown）。
    /// 證據與重送分家（AGM 2026-09-16 裁示，review 第 3 條）：
    /// `agent.prompt` 沒有證據 → 未驗證，但重送照舊開著；打字證不明 → 未驗證且關掉重送。
    #[test]
    fn evidence_and_auto_resend_are_recorded_separately() {
        let r = |d: Delivered| d.record().expect("a delivered outcome records something");
        assert_eq!(r(Delivered::Submitted), DeliveryRecord { stored: "ok", verified: true, auto_resend: false });
        assert_eq!(r(Delivered::Handed), DeliveryRecord { stored: "ok", verified: false, auto_resend: true });
        assert_eq!(r(Delivered::Unverified), DeliveryRecord { stored: "ok", verified: false, auto_resend: false });
        assert_eq!(r(Delivered::Unproven("x")), DeliveryRecord { stored: "unknown", verified: false, auto_resend: true });
        assert!(Delivered::NotAttempted { reason: "x", retry: true }.record().is_none());
    }
}

#[cfg(any(test, feature = "test-hooks"))]
#[doc(hidden)]
pub mod test_seams {
    //! #1862 L2：daemon 測試要碰的私有項，窄轉呼叫（只在測試建置存在）。
    use super::*;
    pub async fn composer_ready(app: &impl LcHost, run: &db::Run, kind: &str, screen: &str) -> Result<(), Delivered> { super::composer_ready(app, run, kind, screen).await }
    pub fn may_press_retry_submit(kind: &str, screen: &str, text: &str, positive_evidence: bool, pressed_again: bool) -> bool { super::may_press_retry_submit(kind, screen, text, positive_evidence, pressed_again) }
    pub fn composer_still_holds(kind: &str, screen: &str, text: &str) -> bool { super::composer_still_holds(kind, screen, text) }
    pub fn text_still_in_box(kind: &str, screen: &str, text: &str) -> bool { super::text_still_in_box(kind, screen, text) }
}
