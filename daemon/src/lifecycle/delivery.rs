//! Typing a prompt into a pane, and proving it was submitted — without guessing.
//!
//! Why this exists: herdr `agent.prompt` answered ok twice on wits-c1-op-xh (2026-09-14 14:24 and
//! 15:33, the second two minutes after a live `/effort`) while the text never reached the pane.
//! Those runs type into the pane instead, and a delivery only counts when it can be **proven**.
//!
//! What is *not* used as proof any more (sol review rounds one to five): the text as the TUI drew
//! it. A TUI wraps long lines itself, so a row break on screen cannot be told apart from a newline
//! the user typed; herdr reports no column width; trailing spaces are not visible; tabs, emoji,
//! combining marks and ZWJ sequences have no dependable width. Every attempt to rebuild the prompt
//! from rows was lossy somewhere. So the evidence is chosen **before typing**, and only two kinds
//! are accepted:
//!
//! * **one echo row** — for a single-line prompt with no trailing whitespace and no characters of
//!   uncertain width: a submitted message shows as exactly one row `❯ <text>` (or `> <text>`)
//!   with no continuation row under it. No width is needed: a wrapped or multi-line message would
//!   have a continuation row, and then this proof simply does not apply.
//! * **the agent's own transcript** — for claude, a new user entry in the session transcript whose
//!   text is byte-for-byte what was sent. Lossless for any length and any whitespace.
//!
//! When neither applies, nothing is typed and the delivery is `Unknown`.

use super::*;
use std::time::Duration;

/// What the composer holds right now. Ownership is never inferred from the text: the box has to
/// be empty before typing, and after an atomic paste under the bot lock the text in it is ours.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BoxState {
    /// The bare marker row.
    Empty,
    /// Anything typed into it — a draft someone is writing, or our paste.
    NonEmpty,
    /// No readable composer on this screen.
    Unready,
}

/// The outcome of one delivery attempt. Anything short of `Submitted` is not success.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Delivered {
    Submitted,
    /// Could not be proven either way. The caller parks the turn and never re-types on this.
    Unknown(&'static str),
}

/// The evidence a delivery will be judged by, fixed before the first keystroke.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Proof {
    /// Count single echo rows equal to this line.
    EchoRow,
    /// Count transcript user entries equal to the text, in this local file.
    Transcript(std::path::PathBuf),
}

/// Pause after typing before the box is read (the TUI has to draw the paste).
const TYPE_SETTLE_MS: u64 = 700;
/// Pause after Enter before the evidence is checked, and how many times it is re-checked.
const SUBMIT_SETTLE_MS: u64 = 1200;
const SUBMIT_CHECKS: u32 = 3;
/// Rows read from the pane for every check.
const DELIVER_SCAN_LINES: u32 = 400;
/// Longest prompt the daemon will type and try to prove. Beyond this the transcript tail below
/// could not hold the entry, so the delivery is refused up front rather than left unprovable.
pub(crate) const MAX_PROVABLE_CHARS: usize = 200_000;
/// Bytes read from the end of the transcript. Bounded so a long session never costs a full read.
const TRANSCRIPT_TAIL_BYTES: u64 = 2 * 1024 * 1024;

/// The echo markers a kind draws at the start of a submitted message. claude has drawn both `❯ `
/// and `> ` across versions; matched as the exact prefix of the row, never after trimming.
pub(crate) fn echo_markers(kind: &str) -> &'static [&'static str] {
    match kind {
        "claude" => &["❯ ", "> "],
        "grok" => &["❯ "],
        "codex" => &["› "],
        _ => &[],
    }
}

/// Characters whose rendered form is not a dependable copy of the input: control characters
/// (tab included — it expands by column), zero-width joiners and non-joiners, variation
/// selectors, and combining marks.
fn uncertain_char(c: char) -> bool {
    let u = c as u32;
    c.is_control()
        || matches!(u, 0x200B..=0x200F | 0x2028..=0x202E | 0x2060..=0x206F | 0xFEFF)
        || matches!(u, 0xFE00..=0xFE0F | 0xE0100..=0xE01EF)
        || matches!(u, 0x0300..=0x036F | 0x0483..=0x0489 | 0x0591..=0x05BD | 0x0610..=0x061A | 0x064B..=0x065F)
        || matches!(u, 0x1AB0..=0x1AFF | 0x1DC0..=0x1DFF | 0x20D0..=0x20FF | 0xFE20..=0xFE2F)
        || matches!(u, 0x3099..=0x309A)
}

/// Can one echo row prove this prompt? One line, nothing invisible at its end, nothing whose
/// rendering may differ from the input.
pub(crate) fn provable_by_echo_row(text: &str) -> bool {
    !text.is_empty()
        && !text.contains('\n')
        && !text.contains('\r')
        && text == text.trim_end()
        && !text.starts_with(char::is_whitespace)
        && !text.chars().any(uncertain_char)
}

/// Index of the composer's marker row: the last row in the bottom of the screen that starts with
/// one of the kind's markers or is the bare marker.
fn composer_row(kind: &str, lines: &[&str]) -> Option<usize> {
    let markers = echo_markers(kind);
    let from = lines.len().saturating_sub(COMPOSER_TAIL);
    lines[from..]
        .iter()
        .rposition(|l| {
            let t = l.trim_start_matches('│');
            let bare = t.trim_end();
            markers.iter().any(|m| t.starts_with(m) || bare == m.trim_end())
        })
        .map(|i| from + i)
}

/// Is `row` a continuation of the row above it (the TUI's two-column gutter plus text)?
fn continuation_row(row: &str) -> bool {
    row.strip_prefix("  ").map(|rest| !rest.trim().is_empty()).unwrap_or(false)
        && !row.trim_start().starts_with(['⏺', '✻', '⎿', '●', '─', '│'])
}

/// Pure: what the composer holds on this screen.
pub(crate) fn box_state(kind: &str, screen: &str) -> BoxState {
    let lines: Vec<&str> = screen.lines().collect();
    let Some(idx) = composer_row(kind, &lines) else { return BoxState::Unready };
    let row = lines[idx].trim_start_matches('│');
    let after_marker = echo_markers(kind)
        .iter()
        .find_map(|m| row.strip_prefix(m).or_else(|| (row.trim_end() == m.trim_end()).then_some("")))
        .unwrap_or("");
    let more = lines.get(idx + 1).map(|r| continuation_row(r)).unwrap_or(false);
    if after_marker.trim().is_empty() && !more {
        if pane_awaits_input(kind, screen) {
            BoxState::Empty
        } else {
            BoxState::Unready
        }
    } else {
        BoxState::NonEmpty
    }
}

/// How many single, un-continued echo rows above the composer say exactly `line`.
///
/// A row followed by a continuation row is never counted — whether the break under it was a soft
/// wrap or a typed newline cannot be told, and neither reading proves this prompt.
pub(crate) fn echo_row_hits(kind: &str, screen: &str, line: &str) -> usize {
    let lines: Vec<&str> = screen.lines().collect();
    let end = composer_row(kind, &lines).unwrap_or(lines.len());
    let markers = echo_markers(kind);
    (0..end)
        .filter(|&i| {
            let row = lines[i].trim_end();
            let exact = markers.iter().any(|m| row.strip_prefix(m) == Some(line));
            let continued = lines.get(i + 1).map(|r| i + 1 < end && continuation_row(r)).unwrap_or(false);
            exact && !continued
        })
        .count()
}

/// The text of one transcript line if it is a user message typed by a person (not a tool result,
/// not a meta or synthetic entry). Content is taken exactly as stored.
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

/// How many user entries in the tail of `path` are exactly `text`. `Err` when the file cannot be
/// read — the caller must not read that as zero.
pub(crate) fn transcript_hits(path: &std::path::Path, text: &str) -> std::io::Result<usize> {
    use std::io::{Read, Seek, SeekFrom};
    let mut f = std::fs::File::open(path)?;
    let len = f.metadata()?.len();
    let start = len.saturating_sub(TRANSCRIPT_TAIL_BYTES);
    f.seek(SeekFrom::Start(start))?;
    let mut buf = Vec::new();
    f.read_to_end(&mut buf)?;
    let body = String::from_utf8_lossy(&buf);
    // A tail that starts mid-line has a broken first line; it simply fails to parse.
    Ok(body.lines().filter_map(transcript_user_text).filter(|t| t == text).count())
}

/// Choose the evidence for this prompt on this run, or say why there is none.
pub(crate) fn choose_proof(kind: &str, host_is_local: bool, transcript_path: Option<&str>, text: &str) -> Result<Proof, &'static str> {
    if text.chars().count() > MAX_PROVABLE_CHARS {
        return Err("prompt_too_long_to_prove");
    }
    if provable_by_echo_row(text) && !echo_markers(kind).is_empty() {
        return Ok(Proof::EchoRow);
    }
    match (kind, host_is_local, transcript_path.map(str::trim).filter(|p| !p.is_empty())) {
        ("claude", true, Some(path)) if std::path::Path::new(path).is_file() => Ok(Proof::Transcript(path.into())),
        _ => Err("no_lossless_proof"),
    }
}

/// Count the evidence on the current screen / transcript.
fn evidence(kind: &str, proof: &Proof, screen: &str, text: &str) -> std::io::Result<usize> {
    match proof {
        Proof::EchoRow => Ok(echo_row_hits(kind, screen, text)),
        Proof::Transcript(path) => transcript_hits(path, text),
    }
}

/// `agent.prompt` on an agent herdr has no session bound to answered ok while the text never
/// reached the pane (2026-09-14 wits-c1-op-xh). Treat that as "cannot deliver through the agent".
async fn agent_prompt_usable(client: &HerdrClient, target: &str) -> bool {
    match client.agent_get(target).await {
        Ok(Some(info)) => info.agent_session.is_some(),
        _ => false,
    }
}

/// Deliver a prompt to the agent, and know whether it landed.
///
/// `agent.prompt` stays the path for a run whose pane the daemon has not typed into and whose
/// agent herdr has a session bound to. Every other run types into its pane:
///
/// 1. the evidence is chosen first ([`choose_proof`]); no evidence → nothing is typed;
/// 2. the box must be empty; a draft is never cleared or typed over;
/// 3. the prompt is pasted in one write and must show up as a non-empty box — only a box that is
///    still provably empty is typed into a second time;
/// 4. Enter; the box must be empty again and the evidence must have grown by one.
///
/// Read failures are errors, never empty screens. The run is marked before the first keystroke.
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
            return Ok(Delivered::Unknown("no_pane_to_type_into"));
        }
        client
            .call_timeout("agent.prompt", json!({"target": target, "text": text}), Duration::from_secs(10))
            .await?;
        return Ok(Delivered::Submitted);
    };

    // 1. Evidence first: never type what cannot be proven afterwards.
    let host_is_local = match db::project(&app.db, &bot.project_id).await {
        Ok(Some(p)) => p.host == LOCAL_HOST,
        _ => false,
    };
    let proof = match choose_proof(&bot.kind, host_is_local, run.transcript_path.as_deref(), text) {
        Ok(p) => p,
        Err(why) => {
            tracing::warn!(run = %run.id, bot = %bot.name, reason = why, "no lossless way to prove this prompt; not typing it");
            return Ok(Delivered::Unknown(why));
        }
    };

    // Persist "this pane gets typed into" before the first keystroke (sol review round three #2).
    crate::lifecycle::remember_pane_typed(&run.id);
    if let Err(e) = db::set_pane_typed(&app.db, &run.id).await {
        anyhow::bail!("could not record runs.pane_typed for {} before typing into its pane: {e}", run.id);
    }
    let read = || async { client.pane_read(&pane, "recent-unwrapped", DELIVER_SCAN_LINES).await.map(|r| r.text) };

    // 2. The box has to be empty.
    let before = read().await?;
    match box_state(&bot.kind, &before) {
        BoxState::Empty => {}
        BoxState::NonEmpty => return Ok(Delivered::Unknown("composer_busy")),
        BoxState::Unready => return Ok(Delivered::Unknown("composer_unreadable")),
    }
    let baseline = evidence(&bot.kind, &proof, &before, text)?;

    // 3. Paste, and see the box fill.
    client.pane_send_text(&pane, text).await?;
    tokio::time::sleep(Duration::from_millis(TYPE_SETTLE_MS)).await;
    let mut seen = read().await?;
    if box_state(&bot.kind, &seen) == BoxState::Empty && evidence(&bot.kind, &proof, &seen, text)? == baseline {
        tracing::warn!(run = %run.id, bot = %bot.name, "the paste did not reach the composer; pasting once more");
        client.pane_send_text(&pane, text).await?;
        tokio::time::sleep(Duration::from_millis(TYPE_SETTLE_MS)).await;
        seen = read().await?;
    }
    match box_state(&bot.kind, &seen) {
        BoxState::NonEmpty => {}
        BoxState::Empty if evidence(&bot.kind, &proof, &seen, text)? > baseline => {
            tracing::info!(run = %run.id, bot = %bot.name, "the paste was submitted without an Enter");
            return Ok(Delivered::Submitted);
        }
        BoxState::Empty => return Ok(Delivered::Unknown("nothing_typed")),
        BoxState::Unready => return Ok(Delivered::Unknown("composer_unreadable")),
    }

    // 4. Enter, then wait for the evidence. A box that still holds text gets one more Enter.
    client.pane_send_keys(&pane, &["Enter"]).await?;
    let mut pressed_again = false;
    for _ in 0..SUBMIT_CHECKS {
        tokio::time::sleep(Duration::from_millis(SUBMIT_SETTLE_MS)).await;
        let now = read().await?;
        match box_state(&bot.kind, &now) {
            BoxState::Empty if evidence(&bot.kind, &proof, &now, text)? > baseline => {
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
    Ok(Delivered::Unknown(why))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn screen(transcript: &[&str], box_rows: &[&str]) -> String {
        let mut s = String::new();
        for l in transcript {
            s.push_str(l);
            s.push('\n');
        }
        s.push_str("✻ Crunching… (3s · esc to interrupt)\n");
        s.push_str("─────────────────────────────────────────────\n");
        match box_rows.split_first() {
            None => s.push_str("❯\n"),
            Some((first, rest)) => {
                s.push_str(&format!("❯ {first}\n"));
                for r in rest {
                    s.push_str(&format!("  {r}\n"));
                }
            }
        }
        s.push_str("─────────────────────────────────────────────\n");
        s.push_str("  user. | web | OP5 61% | 5h:53% | 7d:95%\n");
        s
    }

    /// 哪些 prompt 可以用「一列回音」證明：只看內容本身，不需要任何欄寬。
    #[test]
    fn which_prompts_one_echo_row_can_prove() {
        let table: &[(&str, bool, &str)] = &[
            ("Reply with PONG", true, "一般單行"),
            ("加一個功能除了按 SKU 之外", true, "CJK 單行"),
            ("看這個 🎉 對不對", true, "單一 emoji（沒有 ZWJ、沒有變體選擇符）"),
            ("go", true, "短"),
            ("line one\nline two", false, "硬換行：一列回音證明不了"),
            ("trailing two spaces  ", false, "行尾空白在畫面上看不見"),
            ("trailing ideographic space\u{3000}", false, "全形空白結尾"),
            ("  indented", false, "開頭縮排可能被 TUI 吃掉"),
            ("tab\there", false, "tab 依欄位展開"),
            ("family 👨\u{200D}👩\u{200D}👧", false, "ZWJ 序列"),
            ("heart ❤\u{FE0F}", false, "變體選擇符"),
            ("e\u{0301}clair", false, "組合字元"),
            ("", false, "空字串"),
        ];
        for (text, want, why) in table {
            assert_eq!(provable_by_echo_row(text), *want, "{why}: {text:?}");
        }
    }

    /// 硬換行和軟折行在畫面上長得一樣：兩種讀法都不能拿來證明（第五輪 #1/#4）。
    #[test]
    fn a_row_with_a_continuation_under_it_proves_nothing() {
        let ambiguous = screen(&["❯ ab", "  cd"], &[]);
        assert_eq!(echo_row_hits("claude", &ambiguous, "ab"), 0, "下面有續行：可能是 ab\\ncd 也可能是 abcd");
        assert_eq!(echo_row_hits("claude", &ambiguous, "abcd"), 0);
        assert!(choose_proof("claude", true, None, "ab\ncd").is_err(), "多行又沒有 transcript：不打字");
        let single = screen(&["❯ ab", "⏺ 好"], &[]);
        assert_eq!(echo_row_hits("claude", &single, "ab"), 1);
    }

    /// 回音列要精確：marker 以原樣前綴比對，內容不 trim，縮排與空白都算。
    #[test]
    fn an_echo_row_matches_exactly_including_the_marker_variant() {
        let s = screen(&["> Reply with PONG", "⏺ PONG"], &[]);
        assert_eq!(echo_row_hits("claude", &s, "Reply with PONG"), 1, "claude 的 `> ` 變體");
        let s = screen(&["❯  Reply with PONG"], &[]);
        assert_eq!(echo_row_hits("claude", &s, "Reply with PONG"), 0, "多一個空白就不是同一句");
        let s = screen(&["❯ Reply with pong"], &[]);
        assert_eq!(echo_row_hits("claude", &s, "Reply with PONG"), 0);
        // 還在輸入框裡的同一句不是回音。
        let s = screen(&[], &["Reply with PONG"]);
        assert_eq!(echo_row_hits("claude", &s, "Reply with PONG"), 0);
    }

    #[test]
    fn the_box_is_empty_nonempty_or_unreadable_and_nothing_else() {
        assert_eq!(box_state("claude", &screen(&["⏺ 先前的回覆"], &[])), BoxState::Empty);
        assert_eq!(box_state("claude", &screen(&[], &["我自己在打的草稿"])), BoxState::NonEmpty);
        assert_eq!(box_state("claude", &screen(&[], &["", "第二行有字"])), BoxState::NonEmpty, "首列空白、續行有字仍然不是空框");
        assert_eq!(box_state("claude", "Select login method:\n  1. Claude account\n"), BoxState::Unready);
    }

    fn user_entry(content: Value) -> String {
        json!({"type": "user", "message": {"role": "user", "content": content}}).to_string()
    }

    /// transcript 是逐位元組比對：縮排、行尾兩空白、空白行、2k 行都原樣保留。
    #[test]
    fn the_transcript_is_compared_byte_for_byte() {
        let dir = std::env::temp_dir().join(format!("am-transcript-{}", db::ulid()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("t.jsonl");
        let code = "修這段：\n\nfn main() {\n    println!(\"hi\");  \n}";
        let long: String = (0..2000).map(|i| format!("  line {i}\n")).collect();
        let lines = [
            user_entry(json!(code)),
            user_entry(json!("修這段：\nfn main() {\nprintln!(\"hi\");\n}")),
            user_entry(json!([{"type": "tool_result", "content": code}])),
            json!({"type": "user", "isMeta": true, "message": {"content": code}}).to_string(),
            user_entry(json!([{"type": "text", "text": long.clone()}])),
            json!({"type": "assistant", "message": {"content": code}}).to_string(),
        ];
        std::fs::write(&path, lines.join("\n")).unwrap();
        assert_eq!(transcript_hits(&path, code).unwrap(), 1, "縮排被抹平、tool_result、meta、assistant 都不算");
        assert_eq!(transcript_hits(&path, &code.replace("  \n", "\n")).unwrap(), 0, "行尾兩空白是內容");
        assert_eq!(transcript_hits(&path, &long).unwrap(), 1, "2k 行照樣逐字比");
        assert!(transcript_hits(&dir.join("missing.jsonl"), code).is_err(), "讀不到是錯誤，不是 0");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn proof_is_chosen_before_typing_and_refused_when_there_is_none() {
        let dir = std::env::temp_dir().join(format!("am-proof-{}", db::ulid()));
        std::fs::create_dir_all(&dir).unwrap();
        let t = dir.join("t.jsonl");
        std::fs::write(&t, "").unwrap();
        let tp = t.to_str().unwrap();
        assert_eq!(choose_proof("claude", true, Some(tp), "go"), Ok(Proof::EchoRow));
        assert_eq!(choose_proof("claude", true, Some(tp), "a\nb"), Ok(Proof::Transcript(t.clone())));
        assert_eq!(choose_proof("claude", false, Some(tp), "a\nb"), Err("no_lossless_proof"), "遠端 transcript 不讀");
        assert_eq!(choose_proof("grok", true, Some(tp), "a\nb"), Err("no_lossless_proof"));
        assert_eq!(choose_proof("claude", true, None, "a\nb"), Err("no_lossless_proof"));
        let huge = "x".repeat(MAX_PROVABLE_CHARS + 1);
        assert_eq!(choose_proof("claude", true, Some(tp), &huge), Err("prompt_too_long_to_prove"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
