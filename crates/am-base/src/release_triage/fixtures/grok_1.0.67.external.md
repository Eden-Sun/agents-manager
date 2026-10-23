# 1.0.67 — 2026-10-21

## Features

- **Worktrees** also use their main checkout's saved "always allow" answers, so you are not asked again for a tool you already allowed there. Set `[features] worktree_inherits_main_approvals = false` to turn this off; the main checkout's "never allow" answers apply either way.
- **Prompt suggestions** follow vim mode: with `[ui].vim_mode` on, `Tab` moves focus to the scrollback instead of accepting the suggestion. `→` or `Ctrl+→` accepts a suggestion, and `Esc` dismisses it.

## Bug Fixes

- **Headless `grok -p` runs** exit when their work is done instead of hanging when a background task or subagent finishes and its output is read in the same turn.
- **The theme picker in Settings** restores your previous theme when you leave it any way other than `Enter`, as `/theme` already does.
- **Quitting a session with hooks configured** is faster: the session-end `Stop` hook runs at the same time as the `SessionEnd` hooks and gets at most 1.5 seconds.


# 1.0.66 — 2026-10-20

## Bug Fixes

- **`/theme`** restores your previous theme when you leave the picker any way other than `Enter`, such as `Esc`, `Ctrl+U`, `Tab` then `Esc`, or opening Settings or the dashboard.
- **Selected and highlighted rows** stay readable on every theme because reverse-video text is never dimmed. This fixes faint descriptions in selected menu rows on the `terminal` theme in terminals such as Kitty.

## Performance

- **`@` file completion** shows results about 5 ms after you type instead of about 40 ms.


# 1.0.65 — 2026-10-20

## Features

- **`/resume` search** finds sessions from other folders, tolerates typos, and also matches the commands, file paths, and replies in a session.
- **`--permission-mode plan`** starts the session in plan mode, as documented, instead of the default mode.
- **The dashboard's `Ctrl+L` folder picker** lists up to 50 recent folders instead of 10.
- **`grok --help`** gives each command and option a one-line summary, and `grok help -h` and `grok help --help` print the help command's own help instead of an error.

## Bug Fixes

- **`/resume` search** keeps a single typed character literal and matches each typed word at the start of a word, so `log fix` no longer matches "Blog prefix cleanup".
- **`DISABLE_TELEMETRY` and `DISABLE_ERROR_REPORTING`** turn telemetry and crash reports off for any non-empty value, including `0` and `false`. A `GROK_TELEMETRY_ENABLED` or `GROK_ERROR_REPORTING` value Grok can't read also counts as off.


# 1.0.64 — 2026-10-19

## Breaking Changes

- **Untrusted folders** run only your organization's managed hooks. User, project, plugin, and agent hooks wait until you trust the folder. Headless sessions treat user hooks as trusted.

## Features

- **Worktree names** typed with `Ctrl+W` keep Unicode letters such as `café`, and a name or ref that starts with `-` is refused.
- **Auto mode** runs a shell command without review only when every command in it matches an allowlist entry; anything else asks first. Permission rules read each word the way bash passes it to the program.
- **`/resume`** loads older sessions when you reach the last row, and shows how many sessions are loaded.
- **Auto mode** runs `make --version`, `cmake --version`, and `just --version` without asking.
- **HTTP connections**, including remote MCP servers, accept gzip, brotli, zstd, and deflate compressed responses.
- **Plugin install suggestions** above the prompt are off when `disable_plugins = true` is set.

## Bug Fixes

- **New worktrees** keep tracked files that match `.gitignore`, and leave out files excluded only by `.git/info/exclude` or `core.excludesFile`.
- **A new session that fails to start** puts what you typed back in the prompt you return to, instead of losing it.
- **Web search** works from a session's first turn, and in subagents, when the search model signs in through an external auth provider.
- **`grok -w`, `grok worktree create`, worktree forks, and subagent worktrees** refuse up front with the sandbox's message when the sandbox profile does not allow creating worktrees.
- **Worktree options** are hidden in `/fork`, `/new`, and the Welcome screen when the sandbox profile does not allow creating worktrees.
- **A worktree name you type** that is already taken is refused instead of getting a numbered suffix, and a name whose folder you deleted by hand can be used again.
- **External OpenTelemetry export** keeps sending logs and metrics when `OTEL_EXPORTER_OTLP_COMPRESSION` is set to `gzip` or `zstd`.
- **`[model.*]` tables** without a context window take the window and API of another table that sends the same model, instead of the 200,000-token fallback.
- **Grok on macOS without a GUI session**, such as over SSH, no longer hangs on its first modifier-key check.
- **Custom models on the Responses API** keep the turn going when a web search call starts without an action.
- **Shell commands** run correctly on zsh when your startup files alias `grep` or replace `run-help`.
- **Voice dictation preview** grows the prompt as you speak and keeps the newest words in view.


# 1.0.63 — 2026-10-17

## Breaking Changes

- **Memory** always uses the concise `MEMORY.md` index of topic titles and paths, and `/memory` lists topics by how often Grok reads them. The `[memory_v2] compact_index_enabled` setting no longer has an effect.

## Features

- **Rex terminal support** reports each open session and each running subagent as its own record, so Rex shows what each one is doing. Set `children = false` under `[ui.notifications.program_status]` to report only the main status.
- **`grok worktree list` and `grok worktree show`** mark a worktree whose folder is missing or whose source repository moved, with a hint on how to repair it.
- **Command hooks** accept an `args` list: Grok runs `command` as one program with those arguments and no shell, and fills hook variables such as `$GROK_SESSION_ID` in each argument.

## Bug Fixes

- **Plan reviews** that open while you are typing keep your text in the review's notes, so a typed letter no longer triggers a plan shortcut such as approve.
- **Plan reviews** at the end of a turn wait until you close an open `/feedback` dialog, question, tool prompt, or queued prompt edit.
- **`grok -w` and `Ctrl+W`** explain that no worktree was made when you run them outside a git repository.
- **`/fork`** keeps the parent session's model and effort when Grok was started with `-m` or `--effort`.
- **Custom models on Chat Completions endpoints outside xAI** no longer fail turns with a tool schema error when MCP servers are connected.
- **Workflow run views** hide each agent's live status text on narrower terminals, so agent names are no longer cut off.


