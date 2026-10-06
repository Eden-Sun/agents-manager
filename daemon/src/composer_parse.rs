//! Terminal screen, composer and prompt suggestion parsing helpers.

use am_ports::CaptureParser;
use anyhow::Result;
use crate::herdr::{HerdrClient, HerdrError, PaneRead};
use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

pub(crate) fn is_box_bottom(row: &str) -> bool {
    let t = row.trim();
    t.starts_with('╰') && t.ends_with('╯')
}

/// The marker glyph a kind's composer row starts with.
fn composer_glyph(kind: &str) -> Option<char> {
    match kind {
        "claude" | "grok" => Some('❯'),
        "codex" => Some('›'),
        "agy" => Some('>'),
        _ => None,
    }
}

/// One row of a screen read with `format: ansi`: the visible characters, each with whether it was drawn dim.
pub(crate) fn styled_chars(row: &str) -> Vec<(char, bool)> {
    styled_cells(row).into_iter().map(|c| (c.ch, c.dim)).collect()
}

/// One visible cell of a styled row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Cell {
    pub ch: char,
    pub dim: bool,
    pub fg: bool,
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
    (luma(fg) - bg_l).abs() / marker_contrast < 0.6
}

pub(crate) fn strip_ansi(row: &str) -> String {
    styled_chars(row).into_iter().map(|(c, _)| c).collect()
}

pub(crate) struct ComposerRow {
    pub idx: usize,
    pub boxed: bool,
    pub after_glyph: Vec<(char, bool)>,
    pub marker: Option<Cell>,
}

pub(crate) fn composer_row(kind: &str, lines: &[&str]) -> Option<usize> {
    locate_composer(kind, lines, false, composer_tail(kind, lines)).map(|c| c.idx)
}

pub(crate) const COMPOSER_TAIL: usize = 24;

pub(crate) fn composer_tail(kind: &str, lines: &[&str]) -> usize {
    if kind != "claude" || locate_composer(kind, lines, false, COMPOSER_TAIL).is_some() {
        return COMPOSER_TAIL;
    }
    let rule = |i: usize| is_rule_row(strip_ansi(lines[i]).trim());
    let n = lines.len();
    let Some(bottom) = (n.saturating_sub(COMPOSER_TAIL)..n).rev().find(|&i| rule(i)) else { return COMPOSER_TAIL };
    let Some(top) = (0..bottom).rev().find(|&i| rule(i)) else { return COMPOSER_TAIL };
    let marker = top + 1;
    if marker < bottom && strip_ansi(lines[marker]).trim_start().starts_with('❯') {
        (n - marker).max(COMPOSER_TAIL)
    } else {
        COMPOSER_TAIL
    }
}

fn is_particle(c: &Cell) -> bool {
    ('\u{2800}'..='\u{28FF}').contains(&c.ch) && c.fg && !c.dim
}

fn blank_particles(cells: Vec<Cell>, drop: bool) -> Vec<Cell> {
    if !drop {
        return cells;
    }
    cells.into_iter().map(|c| if is_particle(&c) { Cell { ch: ' ', dim: false, fg: false, fg_rgb: None, bg_rgb: None } } else { c }).collect()
}

pub(crate) fn blank_codex_particles(screen: &str) -> String {
    let styled = screen.contains("\u{1b}[");
    screen.lines().map(|l| blank_particles(styled_cells(l), styled).into_iter().map(|c| c.ch).collect::<String>()).collect::<Vec<_>>().join("\n")
}

pub(crate) fn locate_composer(kind: &str, lines: &[&str], drop_particles: bool, tail: usize) -> Option<ComposerRow> {
    let glyph = composer_glyph(kind)?;
    let from = lines.len().saturating_sub(tail);
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
        let marker = cells.get(i).copied();
        (i < end && cells[i].ch == glyph).then(|| ComposerRow {
            idx,
            boxed,
            after_glyph: cells[i + 1..end].iter().map(|c| (c.ch, is_hint_cell(c, marker.as_ref()))).collect(),
            marker,
        })
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BoxState {
    Empty,
    NonEmpty,
    Unready,
}

pub(crate) fn box_state(kind: &str, screen: &str) -> BoxState {
    let lines: Vec<&str> = screen.lines().collect();
    box_state_within(kind, screen, composer_tail(kind, &lines))
}

pub(crate) fn box_state_within(kind: &str, screen: &str, tail: usize) -> BoxState {
    let lines: Vec<&str> = screen.lines().collect();
    let particles = kind == "codex" && screen.contains("\u{1b}[");
    let Some(c) = locate_composer(kind, &lines, particles, tail) else { return BoxState::Unready };
    let content = marker_row_content(kind, &c, particles);
    let skip = if content.is_empty() {
        0
    } else {
        match hint_rows(&c, &content, &lines, particles) {
            Some(n) => n,
            None => return BoxState::NonEmpty,
        }
    };
    let rest: Vec<String> = lines[c.idx + 1 + skip..]
        .iter()
        .map(|r| blank_particles(styled_cells(r), particles).into_iter().map(|c| c.ch).collect())
        .collect();
    let edge = match (kind, c.boxed) {
        (_, true) => rest.iter().take(tail).position(|r| is_box_bottom(r)),
        ("claude" | "agy", false) => rest.iter().take(tail).position(|r| is_rule_row(r)),
        ("codex", false) => Some(rest.iter().position(|r| r.trim().is_empty() || !r.starts_with("  ")).unwrap_or(rest.len())),
        _ => None,
    };
    match edge {
        Some(0) => BoxState::Empty,
        Some(_) => BoxState::NonEmpty,
        None => BoxState::Unready,
    }
}

pub(crate) fn marker_row_content(kind: &str, c: &ComposerRow, particles: bool) -> Vec<(char, bool)> {
    let mut content = c.after_glyph.clone();
    if matches!(content.first(), Some((' ' | '\u{a0}', _))) {
        content.remove(0);
    }
    if c.boxed || particles || kind == "claude" || kind == "agy" {
        while matches!(content.last(), Some((ch, _)) if ch.is_whitespace()) {
            content.pop();
        }
    }
    content
}

pub(crate) fn hint_rows(c: &ComposerRow, content: &[(char, bool)], lines: &[&str], particles: bool) -> Option<usize> {
    let visible: Vec<bool> = content.iter().filter(|(ch, _)| !ch.is_whitespace()).map(|(_, hint)| *hint).collect();
    if visible.is_empty() || !visible.iter().all(|hint| *hint) {
        return None;
    }
    let more = lines[c.idx + 1..]
        .iter()
        .take_while(|r| {
            let cells = blank_particles(styled_cells(r), particles);
            let row: String = cells.iter().map(|x| x.ch).collect();
            if is_rule_row(&row) || is_box_bottom(&row) {
                return false;
            }
            let shown: Vec<&Cell> = cells.iter().filter(|x| !x.ch.is_whitespace() && x.ch != '│').collect();
            !shown.is_empty() && shown.iter().all(|x| is_hint_cell(x, c.marker.as_ref()))
        })
        .count();
    Some(more)
}

pub(crate) fn plain_without_hints(kind: &str, screen: &str) -> String {
    if !screen.contains('\u{1b}') {
        return screen.to_string();
    }
    let lines: Vec<&str> = screen.lines().collect();
    let particles = kind == "codex";
    let mut plain: Vec<String> = lines.iter().map(|l| strip_ansi(l)).collect();
    if let Some(c) = locate_composer(kind, &lines, particles, composer_tail(kind, &lines)) {
        let content = marker_row_content(kind, &c, particles);
        if let Some(n) = hint_rows(&c, &content, &lines, particles) {
            let cells = blank_particles(styled_cells(lines[c.idx]), particles);
            let glyph = composer_glyph(kind);
            let at = cells.iter().position(|x| Some(x.ch) == glyph).unwrap_or(0);
            let row: String =
                cells.iter().enumerate().map(|(i, x)| if i > at && is_hint_cell(x, c.marker.as_ref()) { ' ' } else { x.ch }).collect();
            plain[c.idx] = if c.boxed { row } else { row.trim_end().to_string() };
            plain.drain(c.idx + 1..c.idx + 1 + n);
        }
    }
    plain.join("\n")
}

pub(crate) const SUGGESTION_MAX_CHARS: usize = 500;

pub(crate) fn prompt_suggestion(kind: &str, screen: &str) -> Option<String> {
    if kind != "claude" || !screen.contains('\u{1b}') || box_state(kind, screen) != BoxState::Empty {
        return None;
    }
    let lines: Vec<&str> = screen.lines().collect();
    let c = locate_composer(kind, &lines, false, composer_tail(kind, &lines))?;
    let content = marker_row_content(kind, &c, false);
    let more = hint_rows(&c, &content, &lines, false)?;
    let mut text: String = content.iter().map(|(ch, _)| *ch).collect::<String>().trim().to_string();
    for row in &lines[c.idx + 1..c.idx + 1 + more] {
        let cells: String = styled_cells(row).into_iter().map(|x| x.ch).filter(|ch| *ch != '│').collect();
        let part = cells.trim();
        if part.is_empty() {
            continue;
        }
        if text.chars().next_back().is_some_and(|a| a.is_ascii()) && part.chars().next().is_some_and(|b| b.is_ascii()) {
            text.push(' ');
        }
        text.push_str(part);
    }
    (!text.is_empty() && text.chars().count() <= SUGGESTION_MAX_CHARS).then_some(text)
}

pub(crate) fn is_rule_row(s: &str) -> bool {
    !s.is_empty() && s.chars().all(|c| "─━-=_╭╮╰╯".contains(c))
}

pub(crate) fn strip_grok_decor(line: &str) -> String {
    let mut s = line.trim_end().trim_end_matches('█').trim_end().to_string();
    if let Some(rest) = s.strip_suffix(" AM").or_else(|| s.strip_suffix(" PM")) {
        if let Some((head, clock)) = rest.rsplit_once(' ') {
            let ok = clock.len() >= 4
                && clock.len() <= 5
                && clock.chars().filter(|c| *c == ':').count() == 1
                && clock.chars().all(|c| c.is_ascii_digit() || c == ':');
            if ok && head.ends_with(' ') {
                s = head.trim_end().to_string();
            }
        }
    }
    s
}

pub(crate) fn prompt_echo_prefix(kind: &str) -> Option<&'static str> {
    match kind {
        "claude" | "grok" => Some("❯ "),
        "codex" => Some("› "),
        "agy" => Some("> "),
        _ => None,
    }
}

pub(crate) fn pane_awaits_input(kind: &str, text: &str) -> bool {
    if kind == "claude" {
        return crate::capture::claude::PARSER.awaits_input(text);
    }
    let Some(marker) = prompt_echo_prefix(kind).and_then(|p| p.trim_end().chars().next()) else { return false };
    text.lines().rev().take(12).any(|l| {
        let mut chars = l.chars().filter(|c| !c.is_whitespace() && !"│┃╭╮╰╯─━".contains(*c));
        chars.next() == Some(marker) && chars.next().is_none()
    })
}

pub(crate) fn undecorate_row(line: &str) -> String {
    let s = strip_grok_decor(line);
    s.trim().trim_start_matches('│').trim_end_matches('│').trim().to_string()
}

pub(crate) fn composer_text(kind: &str, screen: &str) -> Option<String> {
    composer_text_rows(kind, screen, false)
}

pub(crate) fn composer_text_whole(kind: &str, screen: &str) -> Option<String> {
    composer_text_rows(kind, screen, kind == "claude")
}

fn composer_text_rows(kind: &str, screen: &str, keep_blank_rows: bool) -> Option<String> {
    let plain = plain_without_hints(kind, screen);
    let screen = plain.as_str();
    let marker = prompt_echo_prefix(kind)?.trim_end();
    if pane_awaits_input(kind, screen) {
        return None;
    }
    let lines: Vec<&str> = screen.lines().collect();
    let from = lines.len().saturating_sub(composer_tail(kind, &lines));
    let tail = &lines[from..];
    let idx = tail.iter().rposition(|l| {
        let t = undecorate_row(l);
        t.starts_with(marker) && !t[marker.len()..].trim().is_empty()
    })?;
    let mut out: Vec<String> = Vec::new();
    for (n, line) in tail[idx..].iter().enumerate() {
        let row = undecorate_row(line);
        let body = if n == 0 { row[marker.len()..].trim().to_string() } else { row };
        if n > 0 && ((body.is_empty() && !keep_blank_rows) || is_rule_row(&body) || is_box_bottom(&body)) {
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

pub(crate) fn ansi_unsupported(e: &anyhow::Error) -> bool {
    e.downcast_ref::<HerdrError>().map(|h| h.message.to_ascii_lowercase().contains("format")).unwrap_or(false)
}

pub(crate) const PLAIN_FOR_ANSI_WARN_SECS: u64 = 600;
static PLAIN_FOR_ANSI_WARNED: OnceLock<Mutex<HashMap<String, Instant>>> = OnceLock::new();

pub(crate) fn should_warn_plain_for_ansi(pane: &str) -> bool {
    let Ok(mut seen) = PLAIN_FOR_ANSI_WARNED.get_or_init(Default::default).lock() else { return false };
    warn_due(&mut seen, pane, Instant::now())
}

pub(crate) fn warn_due(seen: &mut HashMap<String, Instant>, pane: &str, now: Instant) -> bool {
    let window = Duration::from_secs(PLAIN_FOR_ANSI_WARN_SECS);
    seen.retain(|_, at| now.duration_since(*at) < window);
    if seen.contains_key(pane) {
        return false;
    }
    seen.insert(pane.to_string(), now);
    true
}

pub(crate) async fn read_styled_snapshot(
    client: &HerdrClient,
    pane: &str,
    source: &str,
    lines: u32,
) -> Result<PaneRead> {
    match client.pane_read_ansi(pane, source, lines).await {
        Ok(r) => {
            if r.format != "ansi" && should_warn_plain_for_ansi(pane) {
                tracing::warn!(pane, format = %r.format, "asked herdr for a styled read and got plain text; placeholders will read as busy");
            }
            Ok(r)
        }
        Err(e) if ansi_unsupported(&e) => {
            if should_warn_plain_for_ansi(pane) {
                tracing::warn!(pane, error = %e, "herdr has no styled pane.read; using the plain read");
            }
            Ok(client.pane_read(pane, source, lines).await?)
        }
        Err(e) => Err(e),
    }
}

pub(crate) async fn read_styled(client: &HerdrClient, pane: &str, source: &str, lines: u32) -> Result<String> {
    Ok(read_styled_snapshot(client, pane, source, lines).await?.text)
}
