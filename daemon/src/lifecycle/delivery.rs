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
pub(crate) struct DeliveryRecord {
    /// 寫進 `turns.delivery`（CHECK 只認 pending/ok/unknown/failed）。
    pub stored: &'static str,
    pub verified: bool,
    pub auto_resend: bool,
}

impl Delivered {
    /// `None` = 這個結果不會寫 delivery（`NotAttempted` 的 turn 會被撤回）。
    pub(crate) fn record(&self) -> Option<DeliveryRecord> {
        match self {
            Delivered::Submitted => Some(DeliveryRecord { stored: "ok", verified: true, auto_resend: false }),
            Delivered::Handed => Some(DeliveryRecord { stored: "ok", verified: false, auto_resend: true }),
            Delivered::Unverified => Some(DeliveryRecord { stored: "ok", verified: false, auto_resend: false }),
            // unknown 不會被重送（閘門要 delivery=ok），所以不動重送額度。
            Delivered::Unproven(_) => Some(DeliveryRecord { stored: "unknown", verified: false, auto_resend: true }),
            Delivered::NotAttempted { .. } => None,
        }
    }
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

/// 把框裡那句送出去的鍵。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Submit {
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
    pub(crate) fn keys(self) -> &'static [&'static str] {
        match self {
            Submit::Enter => &["Enter"],
            Submit::SendNow => &["ctrl+x", "ctrl+s"],
        }
    }
}

/// How the prompt will be delivered, decided before anything is written.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Plan {
    AgentPrompt { target: String },
    Type { pane: String, proof: Proof, submit: Submit },
}

impl Plan {
    /// 換掉送出鍵（issue #103）。`AgentPrompt` 那條路沒有鍵可以按——插隊送出一律帶 `force_pane`，
    /// 所以計畫必然是 `Type`；真的拿到 `AgentPrompt` 時原樣回傳，讓它走一般送出而不是悄悄插隊。
    pub(crate) fn submitting_with(self, submit: Submit) -> Self {
        match self {
            Plan::Type { pane, proof, .. } => Plan::Type { pane, proof, submit },
            other => other,
        }
    }
}

const TYPE_SETTLE_MS: u64 = 700;
const SUBMIT_SETTLE_MS: u64 = 1200;
const SUBMIT_CHECKS: u32 = 3;
const DELIVER_SCAN_LINES: u32 = 400;
/// The `pane.read` source for delivery scans. herdr only knows `visible | recent | recent_unwrapped |
/// detection` — the hyphenated spelling is an `invalid_request` (4713c5c P0).
pub(crate) const SCAN_SOURCE: &str = "recent_unwrapped";
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

/// A full-width rule the TUI draws above and below claude's current composer.
fn is_rule_row(row: &str) -> bool {
    let t = row.trim();
    !t.is_empty() && t.chars().all(|c| "─━".contains(c))
}

/// The bottom edge of a boxed composer: `╰──…──╯`, which grok fills with its model label
/// (`╰── Grok 4.6 (low) · always-approve ─╯`).
fn is_box_bottom(row: &str) -> bool {
    let t = row.trim();
    t.starts_with('╰') && t.ends_with('╯')
}

/// The marker glyph a kind's composer row starts with.
fn composer_glyph(kind: &str) -> Option<char> {
    match kind {
        "claude" | "grok" => Some('❯'),
        "codex" => Some('›'),
        _ => None,
    }
}

/// One row of a screen read with `format: ansi`: the visible characters, each with whether it was
/// drawn dim. SGR is followed exactly: `0`/empty resets, `2` sets dim, `22` clears it, and the
/// arguments of `38`/`48` colour selectors (`;5;n`, `;2;r;g;b`) are skipped so their `2` is never
/// mistaken for dim. Other escape sequences are dropped.
pub(crate) fn styled_chars(row: &str) -> Vec<(char, bool)> {
    styled_cells(row).into_iter().map(|c| (c.ch, c.dim)).collect()
}

/// One visible cell of a styled row: the character, whether it is dim, and whether an explicit
/// foreground colour (`30–37`, `90–97`, `38;5;n`, `38;2;r;g;b`) is in effect.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Cell {
    pub ch: char,
    pub dim: bool,
    pub fg: bool,
    /// 真彩／256 色的前景與背景（`38;2;r;g;b`、`48;2;r;g;b`、`38;5;n` 的灰階段）。
    ///
    /// grok 的建議句**不是** SGR 2 的 dim，而是直接畫一個暗灰前景（實測 `38;2;88;88;88`，marker
    /// `❯` 是 `200;200;200`、真的打的字是 `225;225;225`）。只看 `dim` 會把它當成使用者打的草稿，
    /// 那顆 bot 從此每一則 prompt 都 409 `composer_busy`（2026-09-19 w168:p7J）。
    pub fg_rgb: Option<(u8, u8, u8)>,
    pub bg_rgb: Option<(u8, u8, u8)>,
}

pub(crate) fn styled_cells(row: &str) -> Vec<Cell> {
    let mut out = Vec::new();
    let mut dim = false;
    let mut fg = false;
    let mut fg_rgb: Option<(u8, u8, u8)> = None;
    let mut bg_rgb: Option<(u8, u8, u8)> = None;
    let mut it = row.chars().peekable();
    while let Some(c) = it.next() {
        if c != '\u{1b}' {
            if c != '\r' {
                out.push(Cell { ch: c, dim, fg, fg_rgb, bg_rgb });
            }
            continue;
        }
        if it.peek() != Some(&'[') {
            it.next();
            continue;
        }
        it.next();
        let mut params = String::new();
        let mut fin = None;
        for d in it.by_ref() {
            if ('@'..='~').contains(&d) {
                fin = Some(d);
                break;
            }
            params.push(d);
        }
        if fin != Some('m') {
            continue;
        }
        let nums: Vec<&str> = params.split(';').collect();
        let mut i = 0;
        while i < nums.len() {
            match nums[i] {
                "" | "0" => {
                    dim = false;
                    fg = false;
                    fg_rgb = None;
                    bg_rgb = None;
                }
                "2" => dim = true,
                "22" => dim = false,
                "39" => {
                    fg = false;
                    fg_rgb = None;
                }
                "49" => bg_rgb = None,
                n if matches!(n.parse::<u8>(), Ok(30..=37 | 90..=97)) => fg = true,
                "38" | "48" | "58" => {
                    let is_fg = nums[i] == "38";
                    if is_fg {
                        fg = true;
                    }
                    let rgb = match nums.get(i + 1).copied() {
                        Some("2") => {
                            let c = |k: usize| nums.get(i + 2 + k).and_then(|v| v.parse::<u8>().ok());
                            match (c(0), c(1), c(2)) {
                                (Some(r), Some(g), Some(b)) => Some((r, g, b)),
                                _ => None,
                            }
                        }
                        Some("5") => nums.get(i + 2).and_then(|v| v.parse::<u8>().ok()).map(xterm256_rgb),
                        _ => None,
                    };
                    if nums[i] != "58" {
                        if is_fg {
                            fg_rgb = rgb;
                        } else {
                            bg_rgb = rgb;
                        }
                    }
                    i += match nums.get(i + 1).copied() {
                        Some("5") => 2,
                        Some("2") => 4,
                        _ => 0,
                    };
                }
                _ => {}
            }
            i += 1;
        }
    }
    out
}

/// `38;5;n` 的近似 RGB（只要能比亮度就夠，不必精準）。
fn xterm256_rgb(n: u8) -> (u8, u8, u8) {
    match n {
        0..=7 => [(0, 0, 0), (128, 0, 0), (0, 128, 0), (128, 128, 0), (0, 0, 128), (128, 0, 128), (0, 128, 128), (192, 192, 192)][n as usize],
        8..=15 => [(128, 128, 128), (255, 0, 0), (0, 255, 0), (255, 255, 0), (0, 0, 255), (255, 0, 255), (0, 255, 255), (255, 255, 255)][(n - 8) as usize],
        16..=231 => {
            let v = n - 16;
            let step = |x: u8| if x == 0 { 0u8 } else { 55 + 40 * x };
            (step(v / 36), step((v / 6) % 6), step(v % 6))
        }
        _ => {
            let g = 8 + 10 * (n - 232);
            (g, g, g)
        }
    }
}

fn luma((r, g, b): (u8, u8, u8)) -> f32 {
    0.2126 * f32::from(r) + 0.7152 * f32::from(g) + 0.0722 * f32::from(b)
}

/// 這個格子是不是 TUI 自己畫的提示（灰字），而不是使用者打的字。
///
/// SGR 2 的 dim 直接算。顏色的部分**跟同一列的 marker 比**：marker（`❯`／`>`）一定是實字的顏色，
/// 提示畫得比它更貼近背景。比相對亮度差，所以深色淺色主題都成立。
pub(crate) fn is_hint_cell(cell: &Cell, marker: Option<&Cell>) -> bool {
    if cell.dim {
        return true;
    }
    let (Some(fg), Some(bg)) = (cell.fg_rgb, cell.bg_rgb.or(marker.and_then(|m| m.bg_rgb))) else { return false };
    let Some(marker_fg) = marker.and_then(|m| m.fg_rgb) else { return false };
    let bg_l = luma(bg);
    let marker_contrast = (luma(marker_fg) - bg_l).abs();
    if marker_contrast <= f32::EPSILON {
        return false;
    }
    // 對比不到 marker 的六成＝提示。實測 grok：提示 88,88,88／marker 200,200,200／底 20,20,20
    // → 0.37；使用者打的字 225,225,225 → 1.15。
    (luma(fg) - bg_l).abs() / marker_contrast < 0.6
}

/// The row with styling removed.
pub(crate) fn strip_ansi(row: &str) -> String {
    styled_chars(row).into_iter().map(|(c, _)| c).collect()
}

/// Where the composer is, and what its marker row holds.
struct ComposerRow {
    idx: usize,
    /// Drawn inside `│ … │`: trailing spaces before the right edge are the box's padding.
    boxed: bool,
    /// Characters after the marker glyph (box edge removed), with their dim flag.
    after_glyph: Vec<(char, bool)>,
}

/// The last row near the bottom whose first visible glyph is the kind's composer marker.
fn composer_row(kind: &str, lines: &[&str]) -> Option<usize> {
    locate_composer(kind, lines, false).map(|c| c.idx)
}

/// codex (gpt-6-astra and later) animates braille particles (`⠁⠂⠄⠈⠐⠠⢀`) across its composer and
/// the rows around it, each in its own colour, never dim — and they land on blank cells, including
/// the space after `›` and the gaps inside a draft. Only a styled read can tell a particle (coloured,
/// not dim) from typed text; a plain read keeps them as characters and stays fail-closed.
fn is_particle(c: &Cell) -> bool {
    ('\u{2800}'..='\u{28FF}').contains(&c.ch) && c.fg && !c.dim
}

/// A particle erased back to the blank cell it was drawn on.
fn blank_particles(cells: Vec<Cell>, drop: bool) -> Vec<Cell> {
    if !drop {
        return cells;
    }
    cells.into_iter().map(|c| if is_particle(&c) { Cell { ch: ' ', dim: false, fg: false, fg_rgb: None, bg_rgb: None } } else { c }).collect()
}

fn locate_composer(kind: &str, lines: &[&str], drop_particles: bool) -> Option<ComposerRow> {
    let glyph = composer_glyph(kind)?;
    let from = lines.len().saturating_sub(COMPOSER_TAIL);
    (from..lines.len()).rev().find_map(|idx| {
        let cells = blank_particles(styled_cells(lines[idx]), drop_particles);
        let mut i = cells.iter().position(|c| !c.ch.is_whitespace())?;
        let boxed = cells[i].ch == '│';
        let mut end = cells.len();
        if boxed {
            i += 1;
            while i < end && cells[i].ch == ' ' {
                i += 1;
            }
            while end > i && cells[end - 1].ch.is_whitespace() {
                end -= 1;
            }
            if end > i && cells[end - 1].ch == '│' {
                end -= 1;
            }
        }
        // marker 是那一列實字的顏色基準：提示畫得比它更貼近背景（`is_hint_cell`）。
        let marker = cells.get(i).copied();
        (i < end && cells[i].ch == glyph).then(|| ComposerRow {
            idx,
            boxed,
            after_glyph: cells[i + 1..end].iter().map(|c| (c.ch, is_hint_cell(c, marker.as_ref()))).collect(),
        })
    })
}

/// Pure: what the composer holds, read against each provider's real frame.
///
/// * claude (current): `❯` + a no-break space between two full-width rules.
/// * claude (older) and grok: `│ ❯ … │` closed by `╰…╯` (grok writes its model into that edge).
/// * codex: an unboxed `› …` row.
///
/// Empty when the marker row holds nothing but the one separator after the glyph (a space or the
/// no-break space claude draws; inside a box, also the box's padding) — or content that is
/// **entirely dim** (a placeholder or suggested prompt, whatever it says), which only a styled (`format: ansi`) read can show. A plain-text read never
/// accepts a placeholder: the same words could have been typed (sol review round nine #2).
pub(crate) fn box_state(kind: &str, screen: &str) -> BoxState {
    let lines: Vec<&str> = screen.lines().collect();
    // codex's braille animation is only separable from typed text on a styled read (see
    // `is_particle`); a plain read is never relaxed.
    let particles = kind == "codex" && screen.contains("\u{1b}[");
    let Some(c) = locate_composer(kind, &lines, particles) else { return BoxState::Unready };
    let mut content = c.after_glyph;
    if matches!(content.first(), Some((' ' | '\u{a0}', _))) {
        content.remove(0);
    }
    // With the particles erased, what trails the marker row is the composer's own padding.
    if c.boxed || particles {
        while matches!(content.last(), Some((ch, _)) if ch.is_whitespace()) {
            content.pop();
        }
    }
    let text: String = content.iter().map(|(ch, _)| *ch).collect();
    // Anything the TUI draws dim after the marker is its own hint — `Try "…"`, codex's example
    // prompts, claude's suggested next prompt — never typed text, whatever the words say. One
    // visible non-dim character makes it a draft (sol review round nine #2).
    let visible: Vec<bool> = content.iter().filter(|(ch, _)| !ch.is_whitespace()).map(|(_, dim)| *dim).collect();
    let dim_placeholder = !visible.is_empty() && visible.iter().all(|dim| *dim);
    if !(text.is_empty() || dim_placeholder) {
        return BoxState::NonEmpty;
    }
    // Where the frame closes. Any row between the marker row and that edge is more of the
    // composer — a blank second line is still something typed.
    let rest: Vec<String> = lines[c.idx + 1..]
        .iter()
        .map(|r| blank_particles(styled_cells(r), particles).into_iter().map(|c| c.ch).collect())
        .collect();
    let edge = match (kind, c.boxed) {
        (_, true) => rest.iter().take(COMPOSER_TAIL).position(|r| is_box_bottom(r)),
        ("claude", false) => rest.iter().take(COMPOSER_TAIL).position(|r| is_rule_row(r)),
        // codex draws no frame: the composer ends at the first row that is not an indented
        // continuation (a blank row, the status line, or the end of the screen).
        ("codex", false) => Some(rest.iter().position(|r| r.trim().is_empty() || !r.starts_with("  ")).unwrap_or(rest.len())),
        _ => None,
    };
    match edge {
        Some(0) => BoxState::Empty,
        Some(_) => BoxState::NonEmpty,
        // An empty-looking marker row with no frame under it: not a screen we know.
        None => BoxState::Unready,
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
    // claude 可能把貼上的 prompt 包成 `<pasted_content>` 才寫進去（#218）；codex 不會。
    let ours = |t: &String| match format {
        LogFormat::Claude => super::pasted_content::is_sent(t, text),
        LogFormat::Codex => t == text,
    };
    Ok(body.lines().filter_map(|l| log_user_text(format, l)).filter(ours).count())
}

/// Find the rollout codex writes for `session_id` under `codex_home/sessions`.
///
/// The date tree is walked newest first, all of it — a session that has run for weeks is still
/// found. Inside a day, several files for the same session (a resume on the same day) resolve to the
/// newest by modification time. The winner is canonicalized and must still lie under the canonical
/// `sessions` root, so a symlinked entry cannot point the proof at some other file.
pub(crate) fn codex_session_log(codex_home: &std::path::Path, session_id: &str) -> Option<std::path::PathBuf> {
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
    /// This prompt has already waited for evidence that is on its way (a codex rollout not written
    /// yet); stop waiting and fall back to what is available now.
    pub waited_for_log: bool,
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
            if let Some(session) = session {
                match i.codex_log.as_ref().filter(|log| log.is_file()) {
                    Some(log) => {
                        return Ok(Proof::Transcript { format: LogFormat::Codex, path: log.clone(), session_id: session.to_string() });
                    }
                    // The session is known, so its rollout is coming: wait for the lossless proof
                    // instead of typing unverified right away (sol review round nine #1). Only a
                    // prompt that already waited falls through.
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
    // A local claude with hooks reports its transcript at SessionStart: worth a short wait, since a
    // lossless proof is coming. Everything else has no lossless evidence to wait for.
    if i.kind == "claude" && i.host_is_local && i.hooks && (session.is_none() || path.is_none()) {
        return Err(Delivered::NotAttempted { reason: "transcript_not_ready", retry: true });
    }
    Ok(Proof::Unverified)
}

/// herdr refused the `format` parameter itself (an older or remote herdr), as opposed to failing to
/// read the pane.
fn ansi_unsupported(e: &anyhow::Error) -> bool {
    // Only an error that names the parameter: a generic `invalid_params` could be anything.
    e.downcast_ref::<HerdrError>().map(|h| h.message.to_ascii_lowercase().contains("format")).unwrap_or(false)
}

/// Seconds between warnings that herdr answered a styled read with plain text.
const PLAIN_FOR_ANSI_WARN_SECS: u64 = 600;
static PLAIN_FOR_ANSI_WARNED: std::sync::OnceLock<std::sync::Mutex<std::collections::HashMap<String, std::time::Instant>>> =
    std::sync::OnceLock::new();

/// Whether to warn now that `pane` gave plain text for a styled read — at most once per pane every
/// ten minutes, so a herdr that silently ignores `format` is visible without flooding the log.
pub(crate) fn should_warn_plain_for_ansi(pane: &str) -> bool {
    let Ok(mut seen) = PLAIN_FOR_ANSI_WARNED.get_or_init(Default::default).lock() else { return false };
    let now = std::time::Instant::now();
    match seen.get(pane) {
        Some(at) if now.duration_since(*at) < std::time::Duration::from_secs(PLAIN_FOR_ANSI_WARN_SECS) => false,
        _ => {
            seen.insert(pane.to_string(), now);
            true
        }
    }
}

/// Read the pane for the composer checks: styled when herdr can, plain when it cannot. A plain
/// read is the fail-closed fallback — no dim flags, so a placeholder simply reads as `NonEmpty`.
async fn read_composer(client: &HerdrClient, pane: &str) -> anyhow::Result<String> {
    match client.pane_read_ansi(pane, SCAN_SOURCE, DELIVER_SCAN_LINES).await {
        Ok(r) => {
            // A herdr that ignores the parameter answers `format: text`: the read is still usable
            // (placeholders just read as busy), but say so, or the downgrade is invisible.
            if r.format != "ansi" && should_warn_plain_for_ansi(pane) {
                tracing::warn!(pane, format = %r.format, "asked herdr for a styled read and got plain text; placeholders will read as busy");
            }
            Ok(r.text)
        }
        Err(e) if ansi_unsupported(&e) => {
            if should_warn_plain_for_ansi(pane) {
                tracing::warn!(pane, error = %e, "herdr has no styled pane.read; using the plain read");
            }
            Ok(client.pane_read(pane, SCAN_SOURCE, DELIVER_SCAN_LINES).await?.text)
        }
        Err(e) => Err(e),
    }
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
    waited_for_log: bool,
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
    // 讀不到主機就不送（#198）：當成遠端會跳過本機 transcript／rollout 這種無損證據，改成盲打；一個字都還沒打，可重試。
    let host_is_local = match db::project(&app.db, &bot.project_id).await {
        Ok(p) => p.is_some_and(|p| p.host == LOCAL_HOST),
        Err(e) => {
            tracing::warn!(run = %run.id, error = %e, "cannot read the bot's host; not typing without knowing which evidence applies");
            return Ok(Err(Delivered::NotAttempted { reason: "host_unreadable", retry: true }));
        }
    };
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
        waited_for_log,
        pane_cols,
    };
    let proof = match choose_proof(&inputs, text) {
        Ok(p) => p,
        Err(not) => return Ok(Err(not)),
    };
    // Nothing has been typed yet: a pane that cannot be read is "try again later", never a 502
    // and never an unknown delivery (sol review round ten #1).
    let screen = match read_composer(client, &pane).await {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!(run = %run.id, error = %e, "could not read the pane before typing");
            return Ok(Err(Delivered::NotAttempted { reason: "composer_unreadable", retry: true }));
        }
    };
    match box_state(&bot.kind, &screen) {
        BoxState::Empty => Ok(Ok(Plan::Type { pane, proof, submit: Submit::Enter })),
        BoxState::NonEmpty => Ok(Err(Delivered::NotAttempted { reason: "composer_busy", retry: true })),
        BoxState::Unready => Ok(Err(Delivered::NotAttempted { reason: "composer_unreadable", retry: true })),
    }
}

/// Current evidence count for `proof`.
fn evidence(kind: &str, proof: &Proof, offset: u64, screen: &str, text: &str) -> std::io::Result<usize> {
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
    match prepare_delivery(app, client, run, bot, text, plan).await {
        Ok(ready) => type_prepared(app, client, run, bot, text, ready).await,
        Err(not) => Ok(not),
    }
}

/// 按鍵前的準備都做完了：下一步就是第一個按鍵。
pub(crate) enum Ready {
    AgentPrompt { target: String },
    Type { pane: String, proof: Proof, submit: Submit, offset: u64, baseline: usize },
}

/// [`execute_delivery`] 打第一個字之前的那一段：記下「這個 pane 要打字」、重看一次框、取證據基準。
/// 每一種放棄都是 `NotAttempted`（一個鍵都還沒按）。拆出來是給插隊送出用的（#120）：它要在這一段
/// 成功之後才收掉被打斷的那一回合——先收再被擋下的話，claude 其實還在跑，對話裡卻多一筆假的「被打斷」。
pub(crate) async fn prepare_delivery(
    app: &Arc<App>,
    client: &HerdrClient,
    run: &db::Run,
    bot: &db::Bot,
    text: &str,
    plan: Plan,
) -> Result<Ready, Delivered> {
    let (pane, proof, submit) = match plan {
        Plan::AgentPrompt { target } => return Ok(Ready::AgentPrompt { target }),
        Plan::Type { pane, proof, submit } => (pane, proof, submit),
    };
    // Persist "this pane gets typed into" before the first keystroke (sol review round three #2).
    crate::lifecycle::remember_pane_typed(&run.id);
    // 寫不進去時一個字都還沒打：跟其他「打第一個字之前」的失敗一樣是可重試的 NotAttempted。以前回 `Err`，
    // 直接送與排隊都被記成 `delivery='unknown'`，5 分鐘後被 stuck_turns 收成 completed_fallback——工作根本沒送出去
    // （review3 c4 L5）。
    if let Err(e) = db::set_pane_typed(&app.db, &run.id).await {
        tracing::warn!(run = %run.id, error = %e, "could not record runs.pane_typed before typing; nothing was typed");
        return Err(Delivered::NotAttempted { reason: "pane_typed_unwritable", retry: true });
    }

    // The box may have changed since the plan (someone typing in the terminal): look again. Still
    // before the first keystroke, so a failed read is "not attempted".
    let before = match read_composer(client, &pane).await {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!(run = %run.id, error = %e, "could not read the pane before typing");
            return Err(Delivered::NotAttempted { reason: "composer_unreadable", retry: true });
        }
    };
    match box_state(&bot.kind, &before) {
        BoxState::Empty => {}
        BoxState::NonEmpty => return Err(Delivered::NotAttempted { reason: "composer_busy", retry: true }),
        BoxState::Unready => return Err(Delivered::NotAttempted { reason: "composer_unreadable", retry: true }),
    }
    // The baseline is still before the first keystroke: an evidence file that vanished, was swapped
    // or became unreadable since the plan means "not attempted", never an unknown delivery
    // (sol review round eleven). Only failures after `pane_send_text` may become `unknown`.
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
    Ok(Ready::Type { pane, proof, submit, offset, baseline })
}

/// [`execute_delivery`] 從第一個按鍵開始的那一段。這裡之後的放棄都是 `Unproven`／錯誤，不再是 `NotAttempted`。
pub(crate) async fn type_prepared(
    app: &Arc<App>,
    client: &HerdrClient,
    run: &db::Run,
    bot: &db::Bot,
    text: &str,
    ready: Ready,
) -> anyhow::Result<Delivered> {
    match type_text(client, run, bot, text, ready).await? {
        Typing::Done(d) => Ok(d),
        Typing::Ready(t) => {
            press_submit(client, &t).await?;
            confirm_submitted(app, client, run, bot, text, &t).await
        }
    }
}

/// 字已經在框裡，下一步就是送出鍵。
pub(crate) struct Typed {
    pane: String,
    proof: Proof,
    submit: Submit,
    offset: u64,
    baseline: usize,
}

/// [`type_text`] 停下來的地方。
pub(crate) enum Typing {
    /// 字在框裡，接下來是送出鍵。
    Ready(Typed),
    /// 送出鍵之前就有了結論（`Handed`；證據已經長出來＝`Submitted`；框是空的／讀不到＝`Unproven`）。
    Done(Delivered),
}

/// 打字、看框（必要時重貼一次），停在送出鍵之前。插隊送出要在送出鍵上判斷有沒有打斷（#120），所以跟按鍵分開。
pub(crate) async fn type_text(
    client: &HerdrClient,
    run: &db::Run,
    bot: &db::Bot,
    text: &str,
    ready: Ready,
) -> anyhow::Result<Typing> {
    let (pane, proof, submit, offset, baseline) = match ready {
        Ready::AgentPrompt { target } => {
            client
                .call_timeout("agent.prompt", json!({"target": target, "text": text}), Duration::from_secs(10))
                .await?;
            return Ok(Typing::Done(Delivered::Handed));
        }
        Ready::Type { pane, proof, submit, offset, baseline } => (pane, proof, submit, offset, baseline),
    };
    // Styled read when herdr can (`box_state` needs the dim flag); echo evidence strips the styling.
    let read = || read_composer(client, &pane);

    client.pane_send_text(&pane, text).await?;
    tokio::time::sleep(Duration::from_millis(TYPE_SETTLE_MS)).await;
    let mut seen = read().await?;
    let box_empty = box_state(&bot.kind, &seen) == BoxState::Empty;
    let evidence_grew = evidence(&bot.kind, &proof, offset, &seen, text)? > baseline;
    if should_repaste(&proof, box_empty, evidence_grew) {
        tracing::warn!(run = %run.id, bot = %bot.name, "the paste did not reach the composer; pasting once more");
        client.pane_send_text(&pane, text).await?;
        tokio::time::sleep(Duration::from_millis(TYPE_SETTLE_MS)).await;
        seen = read().await?;
    } else if proof == Proof::Unverified && box_empty {
        // 沒有證據的那條路不重貼（見 `should_repaste`），但也不能一格空框就判「沒打進去」：遠端／grok 晚一幀
        // 重畫時字其實已經在框裡，判 `nothing_typed` 會不按 Enter、把字留在框裡，下一則 prompt 因此 409
        // `composer_busy`（review2 deliv 上一輪 #2 的副作用）。多等一個 settle 再看一次。
        tokio::time::sleep(Duration::from_millis(TYPE_SETTLE_MS)).await;
        seen = read().await?;
    }
    match box_state(&bot.kind, &seen) {
        BoxState::NonEmpty => {}
        BoxState::Empty if proof != Proof::Unverified && evidence(&bot.kind, &proof, offset, &seen, text)? > baseline => {
            return Ok(Typing::Done(Delivered::Submitted));
        }
        BoxState::Empty => return Ok(Typing::Done(Delivered::Unproven("nothing_typed"))),
        BoxState::Unready => return Ok(Typing::Done(Delivered::Unproven("composer_unreadable"))),
    }
    Ok(Typing::Ready(Typed { pane, proof, submit, offset, baseline }))
}

/// 按送出鍵。插隊送出時，會打斷正在跑的那一回合的是**這一顆鍵**，不是上面打的字——claude 忙的時候框裡照樣可以打字，
/// 回合照跑（#120）。
pub(crate) async fn press_submit(client: &HerdrClient, t: &Typed) -> anyhow::Result<()> {
    client.pane_send_keys(&t.pane, t.submit.keys()).await
}

/// 送出鍵按下去之後：等證據，字還留在框裡就再按一次。
pub(crate) async fn confirm_submitted(
    app: &Arc<App>,
    client: &HerdrClient,
    run: &db::Run,
    bot: &db::Bot,
    text: &str,
    t: &Typed,
) -> anyhow::Result<Delivered> {
    let Typed { pane, proof, submit, offset, baseline } = t;
    let (offset, baseline) = (*offset, *baseline);
    let read = || read_composer(client, pane);
    let mut pressed_again = false;
    for _ in 0..SUBMIT_CHECKS {
        tokio::time::sleep(Duration::from_millis(SUBMIT_SETTLE_MS)).await;
        let now = read().await?;
        if !same_session(app, &run.id, proof).await {
            tracing::warn!(run = %run.id, bot = %bot.name, "the session changed while delivering; the transcript proof no longer applies");
            return Ok(Delivered::Unproven("session_changed"));
        }
        match box_state(&bot.kind, &now) {
            // No evidence to wait for: the box took the paste and emptied on Enter. That is all
            // that can be said, and it is said as `Unverified`, not as a proven delivery.
            BoxState::Empty if *proof == Proof::Unverified => {
                tracing::warn!(run = %run.id, bot = %bot.name, "prompt typed and submitted; no lossless evidence on this run");
                return Ok(Delivered::Unverified);
            }
            BoxState::Empty if evidence(&bot.kind, proof, offset, &now, text)? > baseline => {
                tracing::info!(run = %run.id, bot = %bot.name, proof = ?proof, "prompt typed into the pane and proven submitted");
                return Ok(Delivered::Submitted);
            }
            // claude 清掉隱形字元、等人 review（#205）：框裡是**清過的**字，再按一次送出去的就不是記下的那一段，
            // 證據永遠對不上。不按，交給使用者看。送出前的清理（`pane_text::for_pane`）本來就不該讓這個畫面出現。
            BoxState::NonEmpty if super::pane_text::invisible_review_notice(&now) => {
                tracing::warn!(run = %run.id, bot = %bot.name, "claude is asking to review a prompt it cleaned of invisible characters; not pressing Enter");
                return Ok(Delivered::Unproven("invisible_chars_review"));
            }
            BoxState::NonEmpty if !pressed_again => {
                tracing::warn!(run = %run.id, bot = %bot.name, ?submit, "prompt still in the composer after the submit key; pressing it again");
                client.pane_send_keys(pane, submit.keys()).await?;
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

/// 送出鍵的 RPC 沒有回、不知道 herdr 按了沒有時，看它到底生效了沒有。
#[derive(Debug)]
pub(crate) enum Landed {
    /// 證據說送出去了。
    Yes(Delivered),
    /// 字還整個留在框裡：鍵沒生效。
    No,
    /// 看不出來（框空了但沒有證據、讀不到）。
    Unknown,
}

/// [`Landed`]：只看、**不按任何鍵**。transcript 是無損證據，出現這一則就是送出去了，不必看畫面。
pub(crate) async fn submit_landed(app: &Arc<App>, client: &HerdrClient, run: &db::Run, bot: &db::Bot, text: &str, t: &Typed) -> Landed {
    for _ in 0..SUBMIT_CHECKS {
        tokio::time::sleep(Duration::from_millis(SUBMIT_SETTLE_MS)).await;
        if matches!(t.proof, Proof::Transcript { .. })
            && same_session(app, &run.id, &t.proof).await
            && evidence(&bot.kind, &t.proof, t.offset, "", text).is_ok_and(|n| n > t.baseline)
        {
            return Landed::Yes(Delivered::Submitted);
        }
        let Ok(now) = read_composer(client, &t.pane).await else { continue };
        match box_state(&bot.kind, &now) {
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
pub(crate) struct SentProof {
    format: LogFormat,
    path: std::path::PathBuf,
    offset: u64,
    baseline: usize,
    text: String,
}

impl SentProof {
    pub(crate) fn of(t: &Typed, text: &str) -> Option<SentProof> {
        match &t.proof {
            Proof::Transcript { format, path, .. } => {
                Some(SentProof { format: *format, path: path.clone(), offset: t.offset, baseline: t.baseline, text: text.to_string() })
            }
            Proof::EchoRow | Proof::Unverified => None,
        }
    }

    /// 現在看得到這一則了嗎？讀不到當成看不到。
    pub(crate) fn shows(&self) -> bool {
        log_hits_since(self.format, &self.path, self.offset, &self.text).is_ok_and(|n| n > self.baseline)
    }
}

/// Plan and execute in one go, for callers that have no turn to hold back (queue flush, resend).
pub(crate) async fn deliver_prompt(
    app: &Arc<App>,
    client: &HerdrClient,
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

#[cfg(test)]
mod tests {
    /// 送達掃描用的 source 必須是 herdr 認得的；本機有 herdr socket 與 pane 時，直接丟給真 herdr 的 RPC
    /// （CLI 的 clap 連字號也收，所以只有 socket 能證明）。
    #[test]
    fn the_scan_source_is_one_real_herdr_accepts() {
        assert!(["visible", "recent", "recent_unwrapped", "detection"].contains(&SCAN_SOURCE));
        let (Ok(sock), Ok(pane)) = (std::env::var("HERDR_SOCKET_PATH"), std::env::var("HERDR_PANE_ID")) else { return };
        use std::io::{BufRead, Write};
        let Ok(mut stream) = std::os::unix::net::UnixStream::connect(&sock) else { return };
        let req = json!({"id": "scan-source", "method": "pane.read",
            "params": {"pane_id": pane, "source": SCAN_SOURCE, "lines": 1, "format": "ansi"}});
        stream.write_all(format!("{req}\n").as_bytes()).unwrap();
        let mut line = String::new();
        std::io::BufReader::new(stream).read_line(&mut line).unwrap();
        let v: Value = serde_json::from_str(&line).unwrap();
        assert!(v.get("error").is_none(), "real herdr rejected source {SCAN_SOURCE}: {v}");
        assert!(v.get("result").is_some(), "{v}");
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

    /// codex v0.154.0（gpt-6-astra）輸入列的點字動畫：2026-09-15 在 herdr pane 開一個 codex、`pane read --format ansi`
    /// 實抓（fixtures/codex_astra_particles_*.ansi）。上下兩列與 marker 列都撒了帶顏色、非 dim 的 ⠁⠂⠄⠈⠐⠠⢀。
    #[test]
    fn codex_braille_particles_are_not_a_draft_on_a_styled_read() {
        assert_eq!(box_state("codex", CODEX_PARTICLES_EMPTY), BoxState::Empty, "只有 dim 佔位字與點字底紋");
        // 2026-09-14 w168:p4R：點字蓋在 `›` 後面那一格上（`›⠁Ask Codex…`）。
        let glued = CODEX_PARTICLES_EMPTY.replacen(
            "\u{1b}[1m\u{1b}[48;2;59;64;76m›\u{1b}[0m\u{1b}[48;2;59;64;76m \u{1b}[0m",
            "\u{1b}[1m\u{1b}[48;2;59;64;76m›\u{1b}[0m\u{1b}[38;2;85;89;99m\u{1b}[48;2;59;64;76m⠁\u{1b}[0m",
            1,
        );
        assert_ne!(glued, CODEX_PARTICLES_EMPTY, "fixture 的 marker 形狀沒變");
        assert_eq!(box_state("codex", &glued), BoxState::Empty, "點字蓋掉 marker 後的空格");
        // 真草稿：點字還蓋在字與字之間的空格上（`login⠁bug`），一般字元照樣算非空。
        assert_eq!(box_state("codex", CODEX_PARTICLES_DRAFT), BoxState::NonEmpty, "點字底紋＋真草稿");
    }

    #[test]
    fn braille_is_only_ignored_when_it_is_a_coloured_codex_particle() {
        // 純文字讀法不放寬：分不出點字是不是打的（sol 第九輪 #2）。
        let plain: String = CODEX_PARTICLES_EMPTY.lines().map(strip_ansi).collect::<Vec<_>>().join("\n");
        assert_eq!(box_state("codex", &plain), BoxState::NonEmpty, "plain read");
        let row = |marker_row: &str, below: &str| format!("\u{1b}[0m\n{marker_row}\n{below}\n\n  gpt-6-astra medium · ~/proj\n");
        let dim_ph = "\u{1b}[1m›\u{1b}[0m \u{1b}[2mAsk Codex to do anything\u{1b}[0m";
        let particle = "\u{1b}[38;2;139;141;148m⠐\u{1b}[0m";
        // 使用者真的打了點字（沒有顏色、非 dim）：是草稿。
        assert_eq!(box_state("codex", &row(&format!("\u{1b}[1m›\u{1b}[0m ⠁⠂"), "")), BoxState::NonEmpty, "uncoloured braille typed");
        // 點字底紋之間混進一個一般字元：草稿。
        assert_eq!(box_state("codex", &row(&format!("{dim_ph}   {particle}  x"), "")), BoxState::NonEmpty, "one ordinary char");
        // 下一列只有縮排＋點字：空白列；下一列有真字：續行。
        assert_eq!(box_state("codex", &row(&format!("{dim_ph}  {particle}"), &format!("      {particle}   {particle}"))), BoxState::Empty);
        assert_eq!(box_state("codex", &row(&format!("{dim_ph}  {particle}"), &format!("  more {particle}"))), BoxState::NonEmpty, "second line typed");
        // claude 不套這條：同樣的點字仍是內容。
        let claude = format!("─────\n❯ {particle}\n─────\n");
        assert_eq!(box_state("claude", &claude), BoxState::NonEmpty);
    }

    #[test]
    /// grok 的建議句是**暗灰前景**（`38;2;88;88;88`），不是 SGR 2 的 dim：只看 dim 會把它當成
    /// 使用者打的草稿，那顆 bot 從此每一則 prompt 都 409 `composer_busy`（2026-09-19 w168:p7J，
    /// 使用者打了字送不出去，畫面上框裡只有一句灰色的「繼續寫完」）。
    #[test]
    fn a_grok_ghost_suggestion_is_a_hint_not_a_draft() {
        // 真畫面的顏色：框線與提示 88,88,88／marker 200,200,200／底 20,20,20。
        let esc = "\u{1b}";
        let bg = format!("{esc}[48;2;20;20;20m");
        let row = |text_fg: &str| {
            format!(
                "{bg}{esc}[38;2;80;80;88m│ {esc}[38;2;200;200;200m❯ {esc}[38;2;{text_fg}m繼續寫完{bg}          {esc}[38;2;80;80;88m│"
            )
        };
        let screen = format!("⏺ 先前的輸出\n{}\n╰──────────────╯\n", row("88;88;88"));
        assert_eq!(box_state("grok", &screen), BoxState::Empty, "灰色建議不是草稿");

        // 同一個框、同樣位置，使用者真的打的字（亮色）就是草稿。
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
        // SGR 2 照舊算提示；沒有顏色資訊（純文字讀）一律不放寬。
        assert!(is_hint_cell(&Cell { ch: 'x', dim: true, fg: false, fg_rgb: None, bg_rgb: None }, None));
        assert!(!is_hint_cell(&Cell { ch: 'x', dim: false, fg: false, fg_rgb: None, bg_rgb: None }, Some(&marker)));
    }

    #[test]
    fn styled_cells_track_the_foreground_colour() {
        let got = styled_cells("a\u{1b}[38;5;2mb\u{1b}[39mc\u{1b}[31md\u{1b}[0me\u{1b}[48;2;1;2;3mf");
        let fg: Vec<(char, bool)> = got.iter().map(|c| (c.ch, c.fg)).collect();
        assert_eq!(fg, vec![('a', false), ('b', true), ('c', false), ('d', true), ('e', false), ('f', false)]);
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
        // 佔位字只有畫成 dim 才算空框；純文字的同一句可能是使用者打的。
        let plain = empty.replacen("❯\n", "❯ Try \"fix lint errors\"\n", 1);
        assert_eq!(box_state("claude", &plain), BoxState::NonEmpty, "純文字的佔位字");
        let dim = empty.replacen("❯\n", "❯ \u{1b}[2mTry \"fix lint errors\"\u{1b}[0m\n", 1);
        assert_eq!(box_state("claude", &dim), BoxState::Empty, "dim 的佔位字");
        // Claude Code 的「建議下一句」：整句 dim，不管寫什麼都是空框；同一句沒 dim、或混進一個非 dim 字就是草稿。
        let suggestion = empty.replacen("❯\n", "❯\u{a0}\u{1b}[2m把 4b 和 4c 補做完\u{1b}[0m\r\n", 1);
        assert_eq!(box_state("claude", &suggestion), BoxState::Empty, "dim 的建議句");
        let typed_same = empty.replacen("❯\n", "❯\u{a0}把 4b 和 4c 補做完\n", 1);
        assert_eq!(box_state("claude", &typed_same), BoxState::NonEmpty, "非 dim 的同一句");
        let mixed = empty.replacen("❯\n", "❯\u{a0}\u{1b}[2m把 4b 和 4c\u{1b}[0m 補做完\n", 1);
        assert_eq!(box_state("claude", &mixed), BoxState::NonEmpty, "混進非 dim 字");
        let dim_codex = "› \u{1b}[2mSomething codex suggests\u{1b}[0m\n\ngpt-6-astra low · ~/proj\n";
        assert_eq!(box_state("codex", dim_codex), BoxState::Empty, "codex 任何 dim 提示");
        let dim_spaces = empty.replacen("❯\n", "❯ \u{1b}[2m  \u{1b}[0m\n", 1);
        assert_eq!(box_state("claude", &dim_spaces), BoxState::NonEmpty, "只有空白不算提示");
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

    /// #218：claude 2.1.278 把**貼上事件**（herdr `agent.prompt`、人在終端貼上）包成 `<pasted_content id=…>` 才寫進
    /// transcript。真 transcript（2026-09-19 實測）：包起來的、20 字門檻下沒包的、字面標籤被 CLI 跳脫的，都認得是同一則；
    /// 只是被包含的一段不算。
    #[test]
    fn a_prompt_the_cli_wrapped_as_pasted_content_is_still_our_prompt() {
        let dir = std::env::temp_dir().join(format!("am-transcript-{}", db::ulid()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("t.jsonl");
        std::fs::write(&path, include_str!("fixtures/claude_2.1.278_pasted_content.jsonl")).unwrap();
        let hits = |text: &str| transcript_hits_since(&path, 0, text).unwrap();
        assert_eq!(hits("請只回覆 OK 兩個字母，不要多說任何其他的話，也不要使用任何工具。"), 1, "整段包起來");
        assert_eq!(hits("請只回覆 OK 兩個字母不要多說其他話"), 1, "19 字：CLI 沒包，照舊逐字");
        assert_eq!(hits("請只回覆 OK 兩個字母，不要多說其他話"), 1, "20 字：包起來");
        assert_eq!(hits("第一行：這是多行貼上測試。\n第二行：請不要使用任何工具。\n第三行：還是一樣。\n第四行：只回覆 OK 兩個字母。"), 1, "多行");
        assert_eq!(
            hits("這段文字裡有字面的 <pasted_content id=\"1234\">x</pasted_content id=\"1234\"> 標籤，請只回覆 OK 兩個字母。"),
            1,
            "字面標籤被 CLI 跳脫成 <\\"
        );
        assert_eq!(hits("請只回覆 OK 兩個字母"), 0, "只是被包含的一段不算");
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn inputs<'a>(kind: &'a str, local: bool, session: Option<&'a str>, path: Option<&'a str>, codex_log: Option<std::path::PathBuf>, cols: Option<u32>) -> ProofInputs<'a> {
        ProofInputs { kind, host_is_local: local, hooks: true, session_id: session, transcript_path: path, codex_log, waited_for_log: false, pane_cols: cols }
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

    #[test]
    fn the_plain_for_ansi_warning_is_throttled_per_pane() {
        let a = format!("pane-throttle-{}", db::ulid());
        let b = format!("pane-throttle-{}", db::ulid());
        assert!(should_warn_plain_for_ansi(&a));
        assert!(!should_warn_plain_for_ansi(&a), "not again within the window");
        assert!(should_warn_plain_for_ansi(&b), "another pane warns on its own");
    }

    /// 只有錯誤訊息明確提到 format 才降級成純文字讀法。
    #[test]
    fn only_an_error_about_format_downgrades_the_read() {
        let e = |code: &str, msg: &str| anyhow::Error::new(HerdrError { code: code.into(), message: msg.into() });
        assert!(ansi_unsupported(&e("invalid_params", "unknown field `format`")));
        assert!(!ansi_unsupported(&e("invalid_params", "lines must be positive")));
        assert!(!ansi_unsupported(&e("unsupported", "pane is gone")));
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
        let home = std::env::temp_dir().join(format!("am-codex-lookup-{}", db::ulid()));
        let day = home.join("sessions/2026/09/14");
        let ancient = home.join("sessions/2026/01/02");
        std::fs::create_dir_all(&day).unwrap();
        std::fs::create_dir_all(&ancient).unwrap();
        let older = day.join("rollout-2026-09-14T08-00-00-sess-same.jsonl");
        let newer = day.join("rollout-2026-09-14T09-00-00-sess-same.jsonl");
        std::fs::write(&newer, "").unwrap();
        std::thread::sleep(std::time::Duration::from_millis(20));
        std::fs::write(&older, "").unwrap();
        // 檔名排序在前的 older 反而是最後寫的：以修改時間為準。
        assert_eq!(codex_session_log(&home, "sess-same"), Some(std::fs::canonicalize(&older).unwrap()));
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

    /// codex session 已知但 rollout 還沒寫出來：先等（可重試），等過了才退回一列回音或 unverified（sol 第九輪 #1）。
    #[test]
    fn a_known_codex_session_waits_for_its_rollout_before_falling_back() {
        let waiting = ProofInputs { waited_for_log: false, ..inputs("codex", true, Some("s1"), None, None, Some(80)) };
        assert_eq!(choose_proof(&waiting, "a\nb"), Err(Delivered::NotAttempted { reason: "codex_log_not_ready", retry: true }));
        assert_eq!(choose_proof(&waiting, "go"), Err(Delivered::NotAttempted { reason: "codex_log_not_ready", retry: true }), "單行也先等無損證據");
        let waited = ProofInputs { waited_for_log: true, ..inputs("codex", true, Some("s1"), None, None, Some(80)) };
        assert_eq!(choose_proof(&waited, "a\nb"), Ok(Proof::Unverified));
        assert_eq!(choose_proof(&waited, "go"), Ok(Proof::EchoRow));
        // session 本身還不知道：沒有東西可等，照舊。
        let unknown = inputs("codex", true, None, None, None, Some(80));
        assert_eq!(choose_proof(&unknown, "a\nb"), Ok(Proof::Unverified));
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
        let canon = |p: std::path::PathBuf| Some(std::fs::canonicalize(p).unwrap());
        assert_eq!(codex_session_log(&home, "01a09ee4-eabb-7682-b363-941a606ed002"), canon(want));
        assert_eq!(codex_session_log(&home, "01a0aaaa-0000"), canon(old.join("rollout-2026-09-01T00-00-00-01a0aaaa-0000.jsonl")));
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

    /// codex 已知 session 但 rollout 還沒寫：直接送的 prompt 回可重試的 409，不建 turn（sol 第九輪 #1）。
    #[tokio::test]
    async fn a_codex_prompt_before_its_rollout_exists_is_a_retryable_409() {
        let env = tt::env().await;
        let app = env.app.clone();
        let (bot_id, conv, run_id) = idle_bot(&env, "codex").await;
        let home = env.dir.join("codex-home-api");
        std::fs::create_dir_all(home.join("sessions")).unwrap();
        sqlx::query("UPDATE bots SET env_json = ? WHERE id = ?").bind(json!({"CODEX_HOME": home.to_str().unwrap()}).to_string()).bind(&bot_id).execute(&app.db).await.unwrap();
        sqlx::query("UPDATE runs SET native_session_id = 'sess-api' WHERE id = ?").bind(&run_id).execute(&app.db).await.unwrap();
        env.herdr.live_pane("pane-api", crate::testing::LivePane { width: Some(120), codex: true, ..Default::default() });
        match prompt(&app, &bot_id, "第一行\n第二行", "crid-codex").await {
            Err(LcError::Conflict(v)) => assert_eq!(v.get("reason").and_then(Value::as_str), Some("codex_log_not_ready")),
            other => panic!("expected 409, got {:?}", other.map(|o| o.delivery)),
        }
        assert_eq!(turns(&app, &conv).await, 0);
        assert_eq!(env.herdr.methods().iter().filter(|m| m.starts_with("pane.send")).count(), 0);
    }

    /// 直接送：herdr 拒絕 `format: ansi` 時退回純文字讀法照樣送出；讀不到畫面時回 409、不建 turn，不是 502。
    #[tokio::test]
    async fn direct_prompts_survive_a_herdr_without_styled_reads_and_unreadable_panes() {
        let env = tt::env().await;
        let app = env.app.clone();
        let (bot_id, conv, _run) = idle_bot(&env, "claude").await;
        env.herdr.reject_ansi.store(true, std::sync::atomic::Ordering::SeqCst);
        env.herdr.live_pane("pane-api", crate::testing::LivePane { width: Some(120), ..Default::default() });
        let out = prompt(&app, &bot_id, "Reply with PONG please", "crid-plain").await.unwrap();
        assert_eq!(out.delivery, "ok");

        let env = tt::env().await;
        let app = env.app.clone();
        // grok 沒有要等的證據，所以會真的走到讀畫面那一步。
        let (bot_id, conv2, _run) = idle_bot(&env, "grok").await;
        env.herdr.set_screen("pane-api", "__READ_ERROR__");
        match prompt(&app, &bot_id, "Reply with PONG please", "crid-unreadable").await {
            Err(LcError::Conflict(v)) => assert_eq!(v.get("reason").and_then(Value::as_str), Some("composer_unreadable")),
            other => panic!("expected a retryable 409, got {:?}", other.map(|o| o.delivery)),
        }
        assert_eq!(turns(&app, &conv2).await, 0);
        let _ = conv;
    }

    /// 證據檔在打字前讀不到（這裡是權限被拿掉）：直接送撤回剛建的 turn、回 409，pane 零寫入（sol 第十一輪）。
    #[cfg(unix)]
    #[tokio::test]
    async fn an_unreadable_evidence_file_withdraws_the_direct_turn() {
        use std::os::unix::fs::PermissionsExt;
        let env = tt::env().await;
        let app = env.app.clone();
        let (bot_id, conv, run_id) = idle_bot(&env, "claude").await;
        let t = env.dir.join("locked.jsonl");
        std::fs::write(&t, "").unwrap();
        sqlx::query("UPDATE runs SET native_session_id = 's-locked', transcript_path = ? WHERE id = ?")
            .bind(t.to_str().unwrap())
            .bind(&run_id)
            .execute(&app.db)
            .await
            .unwrap();
        env.herdr.live_pane("pane-api", crate::testing::LivePane { width: Some(120), ..Default::default() });
        // is_file() 仍然成立，所以規劃會選 transcript；真正讀內容時才失敗。
        std::fs::set_permissions(&t, std::fs::Permissions::from_mode(0o000)).unwrap();
        let res = prompt(&app, &bot_id, "第一行\n第二行", "crid-locked").await;
        std::fs::set_permissions(&t, std::fs::Permissions::from_mode(0o644)).unwrap();
        match res {
            Err(LcError::Conflict(v)) => assert_eq!(v.get("reason").and_then(Value::as_str), Some("transcript_unreadable")),
            other => panic!("expected a retryable 409, got {:?}", other.map(|o| o.delivery)),
        }
        assert_eq!(turns(&app, &conv).await, 0, "the turn was withdrawn, not left unknown");
        assert_eq!(env.herdr.methods().iter().filter(|m| m.starts_with("pane.send")).count(), 0);
    }

    /// grok 多行沒有無損證據：照樣打字送出，回 200 `unverified`，turn 留下「要人工核對」的標記（不是 unknown）。
    /// 證據與重送分家（AGM 2026-09-16 裁示，review 第 3 條）：
    /// `agent.prompt` 沒有證據 → 未驗證，但重送照舊開著；打字證不明 → 未驗證且關掉重送。
    #[test]
    fn evidence_and_auto_resend_are_recorded_separately() {
        let r = |d: Delivered| d.record().expect("a delivered outcome records something");
        // 打字＋無損證據：有證據，而且證據就是「已經進了 session」，重送只會做兩次（review3 c3 M4）。
        assert_eq!(r(Delivered::Submitted), DeliveryRecord { stored: "ok", verified: true, auto_resend: false });
        // agent.prompt：沒有證據（它回 ok 卻沒送進去過），但沒送到才會重送，所以重送安全。
        assert_eq!(r(Delivered::Handed), DeliveryRecord { stored: "ok", verified: false, auto_resend: true });
        // 打過字、證不明：重送會重複派工，關掉。
        assert_eq!(r(Delivered::Unverified), DeliveryRecord { stored: "ok", verified: false, auto_resend: false });
        // unknown 本來就過不了重送閘門（要 delivery=ok），所以不動重送額度。
        assert_eq!(r(Delivered::Unproven("x")), DeliveryRecord { stored: "unknown", verified: false, auto_resend: true });
        assert!(Delivered::NotAttempted { reason: "x", retry: true }.record().is_none());
    }

    #[tokio::test]
    async fn a_grok_multi_line_prompt_is_sent_and_marked_unverified() {
        let env = tt::env().await;
        let app = env.app.clone();
        let (bot_id, conv, _run) = idle_bot(&env, "grok").await;
        env.herdr.live_pane("pane-api", crate::testing::LivePane { width: Some(120), boxed: true, ..Default::default() });

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
