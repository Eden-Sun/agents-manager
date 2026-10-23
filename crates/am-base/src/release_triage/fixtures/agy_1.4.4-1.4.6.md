# Antigravity CLI Changelog

## 1.4.6

- Added a `Configure MCP servers...` row for installed plugins in `/plugin`, so you can set or change a plugin's MCP server variables at any time without disabling and re-enabling it; plugins with missing values show `⚠ needs setup`, each server shows `✓ configured` or the variables it still needs, and saving restarts an enabled plugin's servers so the new values take effect.
- Improved terminal sandbox defaults to protect more credentials: sandboxed commands can no longer read or modify `~/.docker`, `~/.kube`, or `~/_netrc` unless you grant access explicitly. Previously `~/.docker` and `~/.kube` were writable inside the sandbox, so sandboxed commands that use Docker registry or Kubernetes cluster credentials now need an explicit grant.
- Improved context compaction, including `/compact`, to keep images, PDFs, and other files you attached in the conversation summary, so the agent can still see them after older history is summarized.
- Improved permission prompts so that allowing an action for the conversation, in the main agent or in a subagent, applies across the main conversation and all of its subagents instead of asking again for the same action.
- Improved the in-progress status line to show what the agent is waiting on, such as initializing MCP servers or retrying an unavailable model (for example `Model unavailable (attempt 2/10), retrying in 12s` with a live countdown), instead of a generic working label.
- Improved `/diff` to highlight the exact words that changed within each modified line, in every color scheme.
- Improved `agy plugin validate`: it now has `--help`, checks a plugin's context providers (missing `label` or `command`, invalid or duplicate triggers, unknown fields, missing command files), warns about `skills` entries in `plugin.json` that don't exist or contain no `SKILL.md`, and reports every `SKILL.md` the plugin loader skips, including skills listed through `plugin.json` or `skills.json`.
- Fixed terminal commands occasionally failing to start with `argument list too long` when the model returned a very large reasoning signature with the tool call.
- Fixed text cursors drifting away from the insertion point after wide characters (CJK, emoji), tabs, other Unicode spaces, and on soft-wrapped lines, including `Up`/`Down` landing on the wrong row, in the prompt and in the settings, permission amend, plugin setup, feedback, hooks, and artifact search boxes.
- Fixed the `Esc disabled during file edits` warning naming the wrong reject key when the prompt shows an extra choice, such as for edits outside the workspace in accept-edits mode.
- Fixed sign-in not persisting across restarts on macOS and Windows for enterprise accounts, because the saved credential exceeded the system keychain's size limit.

## 1.4.5

- Improved how agent responses read: a blank line now separates the collapsed thought block from the response, and once the response starts, the collapsed thought shows only its `Thought for …` header instead of repeating a snippet of the thinking; expand it to see the full thinking.
- Improved `/` search in the `/diff` file view to count matches inside folded unchanged lines, such as `match 2/5 · +3 hidden` or `no matches · +1 hidden`, and the `f` hint now reads `show N hidden matches`.
- Improved `/diff`'s `Commit Tree` to mark the selected commit with the same `>` cursor and selection color as `All Changes` and `Per Turn` instead of a highlighted bar, and made `Per Turn` start with the cursor on the first file instead of its turn header.
- Improved recurring cron schedules created with the agent's `schedule` tool to fire at a consistent per-schedule offset instead of exactly on the cron boundary, to avoid traffic spikes: schedules that repeat within a day, such as every 30 minutes or hourly, now fire somewhere within their period, fixed-time schedules such as daily at 9:00 still fire within their scheduled minute, and the agent can request exact timing when a schedule needs it. `@every` schedules are unchanged.
- Improved idle CPU usage by pausing screen refreshes while nothing on screen changes, roughly halving the CLI's CPU use when it is idle.
- Improved CPU usage while a response streams by skipping redundant screen updates and refreshing the credit balance less often.
- Improved rendering speed for long conversations, and made long conversations load faster when you resume them.
- Improved startup CPU usage when the CLI refreshes its conversation summaries.
- Improved memory usage by allocating storage for the prompt and other text inputs as you type instead of reserving it up front.
- Fixed the agent silently retrying, potentially without end, when a model response was blocked by content safety filters.
- Fixed plugins failing to load when `plugin.json` gives `skills` as a single path string, such as `"skills": "./skills/"`, instead of an array, as some plugins written for other tools do.
- Fixed marketplace plugin installs and automatic updates failing with a checksum mismatch or network error when a download was briefly stale or interrupted; the CLI now retries the download once.

## 1.4.4

- Added Auto mode on Windows 11 version 24H2 and later for personal Google Accounts, selectable via `--mode auto`, `Agent Mode` in `/config`, `Shift+Tab`, or Remote Control Settings (`Agent` remains the default).
- Added whole-file toggle (`f`) and hunk folding (`Ctrl+O`) to `Commit Tree` and `Per Turn` views in `/diff`, and consolidated `Per Turn` diffs so multiple edits to a file in one turn appear as a single combined diff.
- Added `_meta["antigravity.google/root_conversation_id"]` to MCP `tools/call` requests so MCP servers receive the top-level conversation ID even when a tool is called from a nested subagent.
- Improved Auto mode's review of commands that leave the terminal sandbox to include the assistant's preceding question on short replies like `yes`, check every subcommand in pipelines, and prompt before destructive Git or file commands.
- Improved `agy plugin validate` with stricter checks across `plugin.json`, MCP server configs, `hooks.json`, and sidecar manifests for invalid JSON syntax, unknown fields, bad variable placeholders, unrecognized hook tool matchers, and missing hook scripts.
- Improved CPU and memory usage while streaming responses by caching streamed code-block syntax tokens, rendered Markdown blocks, and inline file-edit confirmation diffs across frames.
- Improved `/compact` and automatic compaction to retain tool declarations on summarization requests so the model API reuses its cached prompt prefix.
- Improved Remote Control on tablets and foldables to use the multi-pane layout with touch-sized controls instead of collapsing to the phone view, and kept popovers open while dragging scrollbars.
- Fixed subagents losing plugins inherited from the parent session, and fixed plugin-bundled subagents with `plugins.inherit_user: false` and restricted MCP tools failing to start with `mcp spec not found for provider`.
- Fixed rules and `SKILL.md` skills loading twice when discovered from both a standalone customization folder and an enabled plugin.
- Fixed long rows wrapping instead of truncating at the terminal width in `/diff`'s `Commit Tree` and `Per Turn` lists, and `Per Turn` headers truncating prompts at 50 characters.
- Fixed the prompt box staying mid-screen after closing panels, dropdowns, or inline prompts, and blank gaps appearing above the prompt in `Medium` verbosity mode.
- Fixed `Ctrl+Z` undo in the prompt mishandling pasted text, merging typing with recalled history entries, recording no-op `Backspace` or `Delete` steps, and truncating characters near emoji.
- Fixed MCP client sessions and background connection routines leaking when `tools/list` failed during server startup or when the CLI shut down while a server was connecting.
- Fixed the system prompt's user rules wording so `GEMINI.md` and `AGENTS.md` rules explicitly override earlier built-in instructions instead of only instructions that follow them.
- Fixed supported media attachments with parameterized MIME types such as `audio/webm; codecs=opus` being rejected as unsupported.
- Fixed several UI issues in Remote Control, including clipped headers and dialogs on phones, iOS Safari zoom on input focus, `Enable Telemetry` and `Marketing Emails` reverting while Settings loads, and dragged URLs losing query or `#` parameters.
- Fixed rendering and export bugs in Remote Control, including broken nested code blocks, `$VAR` shell variables parsed as LaTeX, broken media previews, blurry or blank diagrams in `Copy Image` and `Export as PDF`, and background-tab timer drift.

