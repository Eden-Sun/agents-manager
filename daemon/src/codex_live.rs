//! Changing a **running** codex's model / effort / fast tier (SPEC §4.4a).
//!
//! claude and grok take one slash line and are done (`/model opus`, `/effort high`), which is
//! what [`crate::lifecycle::apply_live_setting`] sends. codex 0.153.4 has the same two things
//! but behind a keyboard UI, verified in a throwaway pane on 2026-09-09:
//!
//! * `/model` — "choose what model and reasoning effort to use". **Takes no arguments**:
//!   `/model gpt-5.6-sol high` is sent to the model as an ordinary prompt (it burns a turn and
//!   changes nothing). Bare `/model` + Enter opens `Select Model and Effort`, a numbered list;
//!   pressing the digit picks the model and immediately opens `Select Reasoning Level for
//!   <model>`, another numbered list; that digit confirms and codex prints
//!   `• Model changed to gpt-5.6-sol high`.
//! * `/fast` — "1.5x speed, increased usage". A plain **toggle**: each Enter flips it and codex
//!   prints `• Service tier set to priority` / `• Service tier set to default`. There is no
//!   "set to X" form, so it may only be sent when the current tier is the wrong one — which is
//!   why `runs.runtime_fast` has to be right before we touch it.
//!
//! Two consequences that shape everything below:
//!
//! 1. **The lists are read, never assumed.** Their contents and order come from the account's
//!    model catalogue, and `(default)` / `(current)` markers move around. Every step reads the
//!    pane back and matches on text.
//! 2. **The picker always asks for both.** Changing only the effort still means picking a model
//!    first, so a bot with no model of its own picks the entry marked `(current)` — the one it
//!    is already on.
//!
//! Side effect worth knowing (same as claude's `/effort`): codex **saves the choice as the
//! account default** in `~/.codex/config.toml`. That is the CLI's behaviour, not ours.

use crate::db;
use crate::herdr::HerdrClient;
use std::time::Duration;

/// What codex's own status line says it is on: `gpt-5.6-sol high fast · /tmp · Context 0% …`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CodexRuntime {
    pub model: String,
    pub effort: Option<String>,
    pub fast: bool,
}

/// The effort as codex spells it in `Select Reasoning Level` (`xhigh` is `Extra high` there).
fn effort_menu_label(effort: &str) -> &'static str {
    match effort {
        "low" => "Low",
        "medium" => "Medium",
        "high" => "High",
        "xhigh" => "Extra high",
        "max" => "Max",
        "ultra" => "Ultra",
        _ => "",
    }
}

/// `max` and `ultra` live behind the `More reasoning…` entry of the first level menu.
fn is_nested_effort(effort: &str) -> bool {
    matches!(effort, "max" | "ultra")
}

/// Parse codex's bottom status line into what it is actually running.
///
/// The line is `<model> [<effort>] [fast] · <cwd> · Context …`, and it is the only place codex
/// states all three at once — which makes it the read-back that proves a live change landed
/// (and the thing the UI has to agree with, SPEC §4.4a).
pub fn parse_status_line(screen: &str) -> Option<CodexRuntime> {
    // Last match wins: the same shape appears in the startup banner (`model: … /model to
    // change`), and the live status line is below it.
    let mut out = None;
    for raw in screen.lines() {
        let line = raw.trim();
        let Some((head, _)) = line.split_once('·') else { continue };
        if !line.contains("Context") {
            continue;
        }
        let mut parts = head.split_whitespace();
        let model = parts.next()?.to_string();
        if !model.contains('-') {
            continue;
        }
        let rest: Vec<&str> = parts.collect();
        let fast = rest.iter().any(|w| *w == "fast");
        let effort = rest
            .iter()
            .find(|w| crate::config::efforts_for_kind("codex").contains(&w.to_ascii_lowercase().as_str()))
            .map(|w| w.to_ascii_lowercase());
        out = Some(CodexRuntime { model, effort, fast });
    }
    out
}

/// The number to press for the entry whose **label** contains `needle`, in a codex picker.
///
/// Lines look like `› 4. gpt-5.6-luna (current)  Fast and affordable…` — the `›` marks the
/// highlighted row, and the description after the two-space gap is not part of the label.
/// That gap matters: row 5 of the level menu is `More reasoning…  Max and Ultra consume usage
/// limits faster`, so a naive substring search for `Max` would pick the submenu row and press
/// it twice instead of once.
pub fn picker_number(screen: &str, needle: &str) -> Option<u32> {
    for raw in screen.lines() {
        let line = raw.trim_start().trim_start_matches('›').trim_start();
        let Some((num, rest)) = line.split_once('.') else { continue };
        let Ok(n) = num.trim().parse::<u32>() else { continue };
        let label = rest.trim_start().split("  ").next().unwrap_or("").trim();
        if label.contains(needle) {
            return Some(n);
        }
    }
    None
}

async fn read(client: &HerdrClient, pane_id: &str) -> String {
    client.pane_read(pane_id, "visible", 60).await.map(|r| r.text).unwrap_or_default()
}

async fn key(client: &HerdrClient, pane_id: &str, k: &str) -> bool {
    client.pane_send_keys(pane_id, &[k]).await.is_ok()
}

async fn text(client: &HerdrClient, pane_id: &str, t: &str) -> bool {
    client.pane_send_text(pane_id, t).await.is_ok()
}

/// Is a `/model` picker (either level) currently on screen?
///
/// The footer is the reliable half: both levels print `Press enter to confirm or esc to go
/// back`, and it survives a narrow pane better than the headings do. The headings are kept as
/// a second way in, since a very short pane can scroll the footer out of the visible rows.
///
/// This is the thing that must be true *before* anyone types into a codex pane: keys sent to
/// an open picker are not text, they are menu navigation — 2026-09-10 the user's message was
/// eaten by one of these and its Enter silently moved the session to another model.
pub fn picker_open(screen: &str) -> bool {
    let t = screen.to_lowercase();
    t.contains("press enter to confirm or esc to go back")
        || t.contains("select model and effort")
        || t.contains("select reasoning level")
}

/// Escapes until no picker is left on screen. Returns `true` when the pane is clear.
///
/// One Escape is **not** enough: codex says `esc to go back`, and from `Select Reasoning
/// Level` it means exactly that — you land back on `Select Model and Effort`, still open. Every
/// failure exit below has to walk all the way out, or the next thing typed into this pane goes
/// into the menu instead of the composer.
pub async fn close_picker(client: &HerdrClient, pane_id: &str) -> bool {
    for _ in 0..PICKER_ESCAPES {
        if !picker_open(&read(client, pane_id).await) {
            return true;
        }
        if !key(client, pane_id, "Escape").await {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(400)).await;
    }
    !picker_open(&read(client, pane_id).await)
}

/// Escapes to spend getting out of the picker: two levels, plus the nested `More reasoning…`
/// one, plus one spare.
const PICKER_ESCAPES: u32 = 4;

/// One picker step: press `n` and give the TUI a moment to redraw. A send that does not go
/// through leaves the menu open, so back out before reporting the failure.
async fn press_number(client: &HerdrClient, pane_id: &str, n: u32) -> bool {
    if !text(client, pane_id, &n.to_string()).await {
        close_picker(client, pane_id).await;
        return false;
    }
    tokio::time::sleep(Duration::from_millis(700)).await;
    true
}

/// Send `/model` and walk the two menus to `(model, effort)`.
///
/// `model: None` keeps whatever the session is on (the `(current)` row); `effort: None` takes
/// the row codex marks `(default)`. Returns `false` the moment a menu does not look the way it
/// should — the caller then reports "needs restart", which is always a safe answer.
async fn apply_model_and_effort(
    client: &HerdrClient,
    pane_id: &str,
    model: Option<&str>,
    effort: Option<&str>,
) -> bool {
    if !text(client, pane_id, "/model").await {
        close_picker(client, pane_id).await;
        return false;
    }
    tokio::time::sleep(Duration::from_millis(600)).await;
    if !key(client, pane_id, "Enter").await {
        close_picker(client, pane_id).await;
        return false;
    }
    tokio::time::sleep(Duration::from_millis(1200)).await;

    let screen = read(client, pane_id).await;
    if !screen.contains("Select Model") {
        // The menu never opened (busy pane, older codex). Leave the composer clean.
        close_picker(client, pane_id).await;
        return false;
    }
    let needle = model.map(str::to_string).unwrap_or_else(|| "(current)".to_string());
    let Some(n) = picker_number(&screen, &needle) else {
        close_picker(client, pane_id).await;
        return false;
    };
    if !press_number(client, pane_id, n).await {
        return false;
    }

    let screen = read(client, pane_id).await;
    if !screen.contains("Select Reasoning Level") {
        close_picker(client, pane_id).await;
        return false;
    }
    let want = effort.unwrap_or("");
    let needle = if want.is_empty() { "(default)".to_string() } else { effort_menu_label(want).to_string() };
    if needle.is_empty() {
        close_picker(client, pane_id).await;
        return false;
    }
    // `max` / `ultra` sit one menu deeper, behind `More reasoning`.
    let screen = if is_nested_effort(want) && picker_number(&screen, &needle).is_none() {
        let Some(more) = picker_number(&screen, "More reasoning") else {
            close_picker(client, pane_id).await;
            return false;
        };
        if !press_number(client, pane_id, more).await {
            return false;
        }
        read(client, pane_id).await
    } else {
        screen
    };
    let Some(n) = picker_number(&screen, &needle) else {
        close_picker(client, pane_id).await;
        return false;
    };
    if !press_number(client, pane_id, n).await {
        return false;
    }
    // The confirming digit closes both menus. If anything is still up, this pane is not safe
    // to hand back — the next prompt would be typed into it — so walk out before saying so.
    close_picker(client, pane_id).await
}

/// Flip the fast tier with `/fast`. It is a toggle, so `want` is only reachable when the
/// session is on the other one — the caller checks that against `runs.runtime_fast`.
async fn toggle_fast(client: &HerdrClient, pane_id: &str) -> bool {
    if !text(client, pane_id, "/fast").await {
        return false;
    }
    tokio::time::sleep(Duration::from_millis(600)).await;
    if !key(client, pane_id, "Enter").await {
        return false;
    }
    tokio::time::sleep(Duration::from_millis(1000)).await;
    true
}

/// Apply `bot`'s model / effort / fast to its running pane, then read the status line back.
///
/// Returns what codex says it is on afterwards, or `None` when anything did not go through —
/// the caller keeps `needs_restart: true` then, which is the honest answer.
pub async fn apply(
    client: &HerdrClient,
    pane_id: &str,
    bot: &db::Bot,
    was_fast: Option<bool>,
    fields: &[&str],
) -> Option<CodexRuntime> {
    if fields.iter().any(|f| *f == "model" || *f == "effort") {
        let model = bot.model.as_deref().map(str::trim).filter(|s| !s.is_empty());
        let effort = bot.effort.as_deref().map(str::trim).filter(|s| !s.is_empty());
        if !apply_model_and_effort(client, pane_id, model, effort).await {
            return None;
        }
    }
    if fields.contains(&"fast") {
        let want = bot.fast != 0;
        // Unknown current tier: a toggle could turn it the wrong way round, so refuse.
        let now = was_fast?;
        if now != want && !toggle_fast(client, pane_id).await {
            return None;
        }
    }
    // Read-back: the status line is codex's own account of all three (SPEC §4.4a).
    tokio::time::sleep(Duration::from_millis(900)).await;
    let seen = parse_status_line(&read(client, pane_id).await)?;
    let want_model = bot.model.as_deref().map(str::trim).filter(|s| !s.is_empty());
    if let Some(m) = want_model {
        if seen.model != m {
            return None;
        }
    }
    if let Some(e) = bot.effort.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        if seen.effort.as_deref() != Some(e) {
            return None;
        }
    }
    if fields.contains(&"fast") && seen.fast != (bot.fast != 0) {
        return None;
    }
    Some(seen)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Real pane text, codex 0.153.4 (2026-09-09).
    const STATUS: &str = "\
╭─────────────────────────────────────────────────────────╮
│ >_ OpenAI Codex (v0.153.4)                              │
│                                                         │
│ model:       gpt-5.6-luna max   fast   /model to change │
│ directory:   /tmp                                       │
╰─────────────────────────────────────────────────────────╯

› Ask Codex to do anything

  gpt-5.6-sol high fast · /tmp · Context 0% used · 5h 82% left · weekly 73% left
";

    const MODEL_MENU: &str = "\
  Select Model and Effort
  Access legacy models by running codex -m <model_name> or in your config.toml

  1. gpt-6-astra (default)   Our most capable model for complex, demanding work.
  2. gpt-5.6-sol             Reliable agentic workhorse for everyday tasks.
  3. gpt-5.6-terra           Balanced agentic coding model for everyday work.
› 4. gpt-5.6-luna (current)  Fast and affordable agentic coding model.
  5. gpt-5.5                 Proven previous-generation model for coding and general work.

  Press enter to confirm or esc to go back
";

    const EFFORT_MENU: &str = "\
  Select Reasoning Level for gpt-5.6-sol

› 1. Low (default)    Fast responses with lighter reasoning
  2. Medium           Balances speed and reasoning depth for everyday tasks
  3. High             Greater reasoning depth for complex problems
  4. Extra high       Extra high reasoning depth for complex problems
  5. More reasoning…  Max and Ultra consume usage limits faster

  Press enter to confirm or esc to go back
";

    #[test]
    fn the_status_line_is_what_codex_is_really_on() {
        let rt = parse_status_line(STATUS).unwrap();
        // The banner above says `gpt-5.6-luna max fast`; the live line below it wins.
        assert_eq!(rt, CodexRuntime { model: "gpt-5.6-sol".into(), effort: Some("high".into()), fast: true });
        let off = parse_status_line("  gpt-5.6-sol high · /tmp · Context 0% used · 5h 82% left\n").unwrap();
        assert!(!off.fast);
        // A model on the CLI default effort prints no level at all.
        let bare = parse_status_line("  gpt-5.5 · /tmp · Context 3% used\n").unwrap();
        assert_eq!(bare.effort, None);
        assert!(parse_status_line("› Ask Codex to do anything\n").is_none());
    }

    #[test]
    fn picker_rows_are_matched_by_text_not_by_position() {
        assert_eq!(picker_number(MODEL_MENU, "gpt-5.6-sol"), Some(2));
        // No model of its own → stay on the row codex marks as current.
        assert_eq!(picker_number(MODEL_MENU, "(current)"), Some(4));
        assert_eq!(picker_number(EFFORT_MENU, "Extra high"), Some(4));
        assert_eq!(picker_number(EFFORT_MENU, "(default)"), Some(1));
        assert_eq!(picker_number(EFFORT_MENU, "More reasoning"), Some(5));
        // `Max` / `Ultra` only appear in row 5's *description*; matching that would press the
        // submenu row as if it were the level itself.
        assert_eq!(picker_number(EFFORT_MENU, "Max"), None, "max / ultra are one menu deeper");
        assert_eq!(picker_number(EFFORT_MENU, "Ultra"), None);
        assert_eq!(picker_number(MODEL_MENU, "gpt-4"), None);
    }

    /// Real composer, codex 0.154.0 (2026-09-10) — nothing in the way.
    const COMPOSER: &str = "\
─ Worked for 1m 17s ────────────────────────────────────

• Model changed to gpt-6-astra high

› Ask Codex to do anything

  gpt-6-astra high · ~/project/agents-manager · Context 44% used · 5h 10% left
";

    /// Both levels of the picker are a menu, not an input box: text sent there is navigation,
    /// and its Enter confirms a row. 2026-09-10 a user's message went into one of these
    /// (`codex-astra`, pane w168:p19) — the message vanished and the session moved to
    /// gpt-5.6-luna medium on its own.
    #[test]
    fn a_pane_showing_a_picker_is_not_ready_for_text() {
        assert!(picker_open(MODEL_MENU));
        assert!(picker_open(EFFORT_MENU));
        // The footer alone is enough: a short pane can scroll the heading away.
        assert!(picker_open("  Press enter to confirm or esc to go back\n"));
        assert!(!picker_open(COMPOSER));
        assert!(!picker_open(STATUS));
        assert!(!picker_open(""));
    }

    #[test]
    fn effort_labels_match_the_menu_codex_draws() {
        assert_eq!(effort_menu_label("xhigh"), "Extra high");
        assert_eq!(effort_menu_label("high"), "High");
        assert!(is_nested_effort("max") && is_nested_effort("ultra"));
        assert!(!is_nested_effort("xhigh"));
    }
}
