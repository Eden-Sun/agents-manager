//! The manager's persona: which copy is authoritative, and which copies are only derived.
//!
//! There were four copies (repo, config.toml, database, the readable file in the manager's
//! directory) and the 2026-09-12 review found two more nobody was counting: the one compiled
//! into the running binary, and the one the live session actually loaded. `ensure_env`
//! unconditionally wrote the binary's copy back over the bot, so an older daemon running
//! `setup` would silently downgrade a persona somebody had just updated.
//!
//! The rule now:
//!
//! - The **stored** persona (in the database) is what the manager runs on.
//! - The **embedded** one seeds it on a first install, and after that only replaces it through
//!   an explicit, recorded migration ([`adopt_embedded`]). `setup` never downgrades.
//! - The readable `persona.md` and the `config.toml` entry are regenerated *from* the stored
//!   version. They are copies; editing them by hand is not the supported path, the API is.
//! - The **loaded** copy — what the running CLI session is actually working from — is not
//!   observable from here. It is reported as `unknown` or `stale`, and `needs_restart == false`
//!   is never dressed up as "the new text is in effect".

use serde_json::{json, Value};

/// FNV-1a, 64-bit. Not a cryptographic hash and not meant to be one: it answers "is this the
/// same text as the one I recorded", and it has to mean the same thing in every build, which
/// rules out `DefaultHasher` (its output is explicitly allowed to change between releases).
pub fn hash(text: &str) -> String {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in text.as_bytes() {
        h ^= u64::from(*b);
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("fnv1a64:{h:016x}")
}

/// Files that are compiled **into** the daemon binary.
///
/// The update script used to diff `daemon web Cargo.toml Cargo.lock` and call everything else
/// "docs only" — but the persona lives in `docs/` and the CLI in `scripts/`, and both are
/// `include_str!`-ed. A change to either needs a rebuild to take effect, which is a different
/// question from whether it is urgent enough to restart anything.
///
/// Kept in sync with the real `include_str!` sites by the test below.
pub const BUILD_INPUTS: [&str; 6] = [
    "daemon",
    "web",
    "Cargo.toml",
    "Cargo.lock",
    "docs/goals/agm-supervisor-persona.md",
    "scripts/agm.py",
];

/// What `GET /api/supervisor/build-inputs` answers.
pub fn build_inputs_json() -> Value {
    json!({
        "paths": BUILD_INPUTS,
        // Said out loud so a caller does not have to guess which of these are the surprising
        // ones: a change here means the *binary* is stale, not that anything must restart now.
        "embedded": [
            {"path": "docs/goals/agm-supervisor-persona.md", "symbol": "supervisor::setup::PERSONA_DOC"},
            {"path": "scripts/agm.py", "symbol": "supervisor::setup::AGM_CLI"},
        ],
        "note": "changes here mean the release binary is behind; when to rebuild or restart is AGM's call",
    })
}

/// How the running session relates to the stored persona.
///
/// Deliberately has no `verified` case. The daemon passes the persona on the command line when
/// it starts the CLI; it cannot see what the session is holding now (a compaction or a `/clear`
/// is invisible from out here), so the best true statement is "it was started with this text".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Loaded {
    /// Nothing is running, so nothing has loaded anything.
    NotRunning,
    /// The session predates the current persona: it is definitely running an older text. This
    /// is the one case we can assert, and it is a negative.
    Stale,
    /// The session was started after the persona was last written, so it was launched with this
    /// text. Whether the model still has it is not observable from here.
    StartedWithCurrent,
}

impl Loaded {
    pub fn as_str(self) -> &'static str {
        match self {
            Loaded::NotRunning => "unknown",
            Loaded::Stale => "stale",
            Loaded::StartedWithCurrent => "unverified",
        }
    }

    /// Only a session that predates the text needs a restart to pick it up.
    pub fn needs_restart(self) -> bool {
        self == Loaded::Stale
    }
}

/// `run_started_at` / `persona_updated_at` are RFC3339; both come straight out of the database.
pub fn loaded_state(run_started_at: Option<&str>, persona_updated_at: Option<&str>) -> Loaded {
    let Some(started) = run_started_at else { return Loaded::NotRunning };
    let Some(updated) = persona_updated_at else { return Loaded::StartedWithCurrent };
    let (Ok(started), Ok(updated)) = (
        chrono::DateTime::parse_from_rfc3339(started),
        chrono::DateTime::parse_from_rfc3339(updated),
    ) else {
        // An unreadable timestamp is not evidence of anything. Say so rather than guessing in
        // the flattering direction.
        return Loaded::NotRunning;
    };
    if started < updated {
        Loaded::Stale
    } else {
        Loaded::StartedWithCurrent
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_hash_is_stable_and_distinguishes_texts() {
        assert_eq!(hash("你是 AGM"), hash("你是 AGM"));
        assert_ne!(hash("你是 AGM"), hash("你是 AGM。"));
        // Pinned: the point of the hash is that it means the same thing in the next build, so a
        // stored value stays comparable after an upgrade.
        assert_eq!(hash(""), "fnv1a64:cbf29ce484222325");
        assert_eq!(hash("a"), "fnv1a64:af63dc4c8601ec8c");
    }

    /// The only honest claims: nothing is running, the session predates the text, or it was
    /// started with it. There is no "verified", because nothing here can see inside a session.
    #[test]
    fn a_running_session_is_never_claimed_to_have_loaded_anything() {
        let old = "2026-09-12T10:00:00Z";
        let new = "2026-09-12T12:00:00Z";
        assert_eq!(loaded_state(None, Some(new)), Loaded::NotRunning);
        assert_eq!(loaded_state(Some(old), Some(new)), Loaded::Stale);
        assert_eq!(loaded_state(Some(new), Some(old)), Loaded::StartedWithCurrent);
        assert_eq!(loaded_state(Some("rubbish"), Some(new)), Loaded::NotRunning, "a broken clock proves nothing");
        assert_eq!(Loaded::StartedWithCurrent.as_str(), "unverified");
        assert!(Loaded::Stale.needs_restart());
        assert!(!Loaded::StartedWithCurrent.needs_restart(), "started with it ≠ must restart");
        assert!(!Loaded::NotRunning.needs_restart());
    }

    /// The build-input list has to keep up with the code, not with somebody remembering to edit
    /// it: every `include_str!` in the daemon must resolve to a path this list covers.
    #[test]
    fn every_embedded_file_is_declared_as_a_build_input() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let mut found = Vec::new();
        let mut stack = vec![root.join("src")];
        while let Some(dir) = stack.pop() {
            for entry in std::fs::read_dir(&dir).expect("daemon/src is readable").flatten() {
                let p = entry.path();
                if p.is_dir() {
                    stack.push(p);
                    continue;
                }
                if p.extension().and_then(|e| e.to_str()) != Some("rs") {
                    continue;
                }
                let text = std::fs::read_to_string(&p).unwrap_or_default();
                for line in text.lines() {
                    let Some(rest) = line.split_once("include_str!(\"") else { continue };
                    let Some((rel, _)) = rest.1.split_once('"') else { continue };
                    // Resolve against the file's own directory, then back to the repo root.
                    let abs = p.parent().expect("file has a parent").join(rel);
                    let Ok(abs) = abs.canonicalize() else { continue };
                    let repo = root.parent().expect("daemon/ has a parent");
                    let Ok(rel_to_repo) = abs.strip_prefix(repo) else { continue };
                    found.push(rel_to_repo.to_string_lossy().replace('\\', "/"));
                }
            }
        }
        for f in &found {
            // A file inside a directory the list already covers (daemon/src/…) is covered.
            let covered = BUILD_INPUTS.iter().any(|p| f == p || f.starts_with(&format!("{p}/")));
            assert!(covered, "{f} is compiled into the binary but is not in BUILD_INPUTS");
        }
        assert!(
            found.iter().any(|f| f == "docs/goals/agm-supervisor-persona.md"),
            "the persona should still be embedded; if that changed, update BUILD_INPUTS and this test. found: {found:?}"
        );
    }
}
