# Capture fixtures

**English** | [繁體中文](README.zh-TW.md)

Versioned samples of CLI terminal screens, compared one by one by the `capture` parser runner. The layout is fixed:

```text
<cli>/<version>/<name>.txt    # raw (or redacted) terminal screen
<cli>/<version>/<name>.toml  # expected values for the screen of the same name
```

Current version directories are `claude/2.1.263`, `codex/0.153.4` and `grok/1.0.13`. `.txt` files must keep the raw Unicode characters the parser relies on (spinner glyphs, `⏺` / `•`, Grok's box borders, etc.); you may remove emails, tokens, real accounts and other sensitive paths, but do not alter the remaining characters for formatting.

## TOML fields

Every `.toml` must have:

```toml
still_busy = false
awaits_input = true
```

Optional fields:

- `reply`: the full reply the parser should extract; omit it when the screen should produce no reply.
- `activity`: the spinner or tool-progress text without the leading glyph, e.g. `Baking…` or `Running 1 shell command…`.
- `noise_lines`: array of 1-based `.txt` line numbers that should be classified as TUI noise; listed lines are excluded entirely, which does not mean blank lines between reply paragraphs should be excluded.
- `limit_hit`: the full text of a limit / usage notice; omit it when there is none.
- `source`: required as `source = "synthetic"` only for non-real screens; real pane snapshots may use `source = "herdr"`.
- `note`: explains the origin, how data was redacted, or why the expected value intentionally differs from existing behavior.

`awaits_input` only means the CLI is waiting for the user to handle a login, permission, menu or other confirmation; an empty composer while working, or returning to an empty composer after a reply, does not count. `reply` and `activity` are compared against the parser output in full, including paragraph line breaks; blank lines inside a reply must not be listed in `noise_lines`.

## Adding a CLI version

1. Run the CLI's `--version` first and create a new `<version>` directory from the actual output; do not overwrite old versions.
2. Take a `recent_unwrapped` / visible snapshot from a Herdr pane, keeping spinners, borders and markers; create synthetic samples only when necessary, adding `source = "synthetic"` and a `note` to the TOML.
3. Add a `.txt` / `.toml` pair for each state, covering at least spinner, tool execution, finished reply, multi-paragraph markers, limit / usage, and login / permission confirmation; for Grok also keep a banner sample.
4. Run the fixture runner; at minimum scan this directory with a TOML parser to confirm every pair exists and `noise_lines` stays within the screen's line count.
