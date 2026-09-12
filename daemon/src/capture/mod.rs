//! Terminal capture parsers, split by CLI.
//!
//! Each CLI owns the rules for its screen vocabulary in one parser. When a CLI version
//! changes, add the real screen to that CLI's versioned fixtures and change only its parser
//! file; lifecycle code should continue to ask this trait the same five questions.

pub mod claude;

/// Provider-specific terminal capture behavior.
pub trait Capture {
    fn still_busy(&self, screen: &str) -> bool;
    fn awaits_input(&self, screen: &str) -> bool;
    fn extract_reply(&self, screen: &str) -> Option<String>;
    fn noise_line(&self, line: &str) -> bool;
    fn activity(&self, screen: &str) -> Option<String>;
}

pub(crate) const ACTIVITY_MAX: usize = 120;

pub(crate) fn is_elapsed_token(s: &str) -> bool {
    let Some(num) = s
        .strip_suffix('s')
        .or_else(|| s.strip_suffix('m'))
        .or_else(|| s.strip_suffix('h'))
    else {
        return false;
    };
    !num.is_empty() && num.chars().all(|c| c.is_ascii_digit() || c == '.')
}

/// Does this row have the shape of a spinner frame — `<Verb>… (3m 18s · ↓ 11.0k tokens)` —
/// whatever glyph, if any, precedes it?
pub(crate) fn is_activity_shape(s: &str) -> bool {
    let body = match s.chars().next() {
        Some(c) if !c.is_alphanumeric() => s[c.len_utf8()..].trim_start(),
        _ => s,
    };
    let Some((verb, tail)) = body.split_once("… (") else {
        return false;
    };
    if verb.is_empty() || verb.chars().any(char::is_whitespace) {
        return false;
    }
    let Some((inner, _)) = tail.rsplit_once(')') else {
        return false;
    };
    inner.contains("tokens")
        || inner
            .split(|c: char| c.is_whitespace() || c == '·')
            .any(|t| is_elapsed_token(t.trim()))
}

/// Has the pane shredded its output into a column of single characters?
pub(crate) fn is_shredded(text: &str) -> bool {
    let lines: Vec<&str> = text
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .collect();
    if lines.len() < 6 {
        return false;
    }
    if lines.iter().map(|l| l.chars().count()).max().unwrap_or(0) <= 6 {
        return true;
    }
    let narrow = lines.iter().filter(|l| l.chars().count() <= 2).count();
    narrow * 10 >= lines.len() * 7
}
