## 1.1.5

- Added a `/effort` command to view and change the current model's reasoning effort, with a left/right timeline-gauge picker and a direct `/effort <level>` form so you can trade latency for depth on the fly.
- Added an `--effort` flag to select a model's reasoning-effort variant when launching the CLI.
- Added stable, user-facing model slugs that appear in the `/model` picker and are accepted by `--model`, so you can pin a specific model reliably across sessions.
- Added a `model` option to custom agent frontmatter so an agent runs at a chosen model tier (such as `flash` or `pro`) when invoked as a subagent, defaulting to `inherit` (the parent's model).
- Redesigned the `/model` picker to group models by their base model and choose reasoning effort from a timeline gauge navigable with Left and Right, and added an effort badge to the status line for models that expose multiple effort variants.
- Improved the `/settings` (`/config`) panel by making it a bounded, scrollable list so it renders correctly in short terminals instead of overflowing, and stopped it from flickering when opening and closing dropdowns.
- Improved background-task reliability by moving long-running work onto a shared lifecycle with deterministic startup and shutdown and panic-safe launching, so a failure in one background task no longer disrupts the session and pending analytics are flushed on exit instead of dropped.
- Improved responsiveness of bursty background refreshes by coalescing rapid repeated triggers into a single run, cutting redundant work.
- Fixed a crash when triggering Authenticate on a remote MCP server in the `/mcp` panel.
- Fixed MCP tool results containing embedded resources being silently dropped, so text and inline media returned by MCP servers now surface in the conversation.
- Fixed permission checks splitting a single command into a pipeline when an argument contained quoted shell metacharacters (such as `--grep="a|b"`), which caused spurious permission prompts.
- Fixed the file-view and file-search tools failing with invalid-UTF-8 errors when a multi-byte character was split at a truncation boundary.
- Fixed a data race when collecting customization rules by guarding the shared structures.

 ## 1.1.4

- Added support for stacking multiple leading slash commands in a single prompt, so a chain like `/plan /grill-me <prompt>` parses, activates, and renders every command in the order you typed them.
- Improved scrolling in the `/diff` viewer so paging through a diff no longer jitters or pushes the status line off the screen when lines wrap or comments expand.
- Fixed custom agents that declare `subagent: false` still appearing in the available-subagents list and being invocable as subagents.
- Fixed headless (`-p` / `--print`) runs so they now honor persisted `settings.json` policies, including `permissions`, file access, sandbox mode, auto-execution, and artifact review.
- Fixed `/btw` side-questions leaking into the conversation list as duplicate entries that carried the parent conversation's title.
- Fixed the prompt to honor a custom Enter binding to `prompt.insert_newline`, so a remapped Enter inserts a newline instead of submitting.
- Fixed eligibility error messages so the CLI shows the real reason again instead of defaulting to a generic "unknown reason".

## 1.1.3

- Added a `/codesearch` command (aliases `/cs` and `/search`) to interactively search code across your workspace, interpreting queries as regex by default with `-F`/`--literal` for exact matching and `f:`/`file:` globs to include or exclude paths.
- Added copy-on-select in no-flickering mode so dragging highlights text and releasing the mouse copies the ANSI-stripped selection to the clipboard, and hides the virtual scrollbar so it no longer interferes with copying multi-line output.
- Added an indicator at each context-compaction boundary so you can see where earlier compaction happened.
- Improved interactive startup latency by loading skills asynchronously so the CLI no longer blocks on a synchronous, filesystem-heavy skill-discovery pass during bring-up.
- Improved eligibility error handling by showing errors with a verification URL inline in the input loop instead of stacking them above the screen.
- Improved customization loading latency for skills, rules, agents, and hooks by consolidating directory walks and caching filesystem lookups to cut redundant I/O during discovery.
- Removed the padding spaces around inline code for tighter rendering.
- Fixed code-block corruption where `$..$` math expansion desynced from the Markdown parser and mangled fenced shell snippets such as `git fetch "$GIT_REMOTE"` by detecting fenced code blocks line-by-line.
- Fixed headless (`-p`) runs hanging or silently auto-approving tools that require a permission confirmation, so the CLI now soft-denies such tools and prints a stderr notice naming the allow-rule needed to permit them.
- Fixed outside-of-workspace file writes being incorrectly auto-approved in always-proceed mode.
- Fixed high CPU and unbounded render cost on large conversations in no-flickering mode by making index rebuilds idempotent so the conversation index converges instead of growing on every rebuild.
- Fixed lingering artifact comments after dismissing the artifact detail view and corrected no-flickering-mode row math so the status line renders correctly within the viewport.
- Fixed repeated sign-in prompts on Linux caused by the OS keyring: the CLI now bypasses the keyring when no D-Bus session bus is present (headless hosts and containers), skips it for an hour after a timeout, and uses longer keyring timeouts so a slow-but-successful credential read is no longer cut short and forced into a fresh login.
- Fixed MCP servers hanging the agent indefinitely when a server never responds by bounding connection, tool-listing, and per-tool-call attempts with timeouts.
- Fixed conversations breaking after certain tool calls, which previously corrupted the conversation history and blocked all further responses.
- Fixed customization rules being loaded twice when a rules directory is reachable through a symlink.

