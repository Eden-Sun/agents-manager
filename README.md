# AG Man (agents-manager)

English | [繁體中文](README.zh-TW.md) | [简体中文](README.zh-CN.md) | [日本語](README.ja.md)

**Run all your coding agents from one web page.**
Claude Code, Codex, Grok, Antigravity, and API models, on your own machines, from your desk or your phone.

<table>
  <tr>
    <td width="75%"><img src="docs/screenshots/readme/en/desktop-light.png" alt="Desktop: bots in the sidebar (running, waiting on you, done), a question card waiting for an answer, quota for every account on top"></td>
    <td width="25%"><img src="docs/screenshots/readme/en/mobile-light.png" alt="Phone: the same question card, answerable with one tap"></td>
  </tr>
</table>

<sub>Screenshots use the built-in mock backend; accounts, bots, and messages are demo data.</sub>

Running several agent CLIs means juggling terminal tabs, missing the one that has been stuck on a yes/no question for an hour, and guessing how much quota is left. AG Man puts every agent session in one self-hosted web UI, tells you when one needs you, and lets agents hand work to each other. It uses the CLI subscriptions you already pay for; API keys are optional.

- **Unblock agents from your phone.** Get a push notification when an agent finishes or asks something, and answer with one tap.
- **See every agent and who needs you.** Each session is a chat, grouped by project, marked running, waiting on you, or done.
- **Know your quota across every account.** Run several subscriptions per CLI; usage, reset times, and Anthropic / OpenAI API balances sit in one strip.
- **Let bots work as a team.** A manager bot hands tasks to worker bots and asks an advisor for a second opinion.
- **Use all your machines.** Projects can live on SSH hosts and work the same as local ones.

## How it works

AG Man is a small daemon (`agents-managerd`) plus a web UI. Agent CLIs run inside [herdr](https://herdr.dev), a terminal workspace manager with an API; the daemon starts herdr and drives it for you, so you never have to touch it. Replies come back through each CLI's hooks, so you get real chat messages and clickable question cards instead of scraped terminal text. API bots run inside the daemon with your API key, no terminal needed.

Sessions live in the daemon, not the browser: close the tab and your agents keep working; open it again on any device and pick up where you left off. AG Man manages the agents; your editor, git, and repo conventions stay as they are.

## Supported agents

| Agent | Notes |
|---|---|
| Claude Code (`claude`) | Anthropic's coding CLI |
| Codex (`codex`) | OpenAI's coding CLI |
| Grok (`grok`) | xAI's coding CLI |
| Antigravity (`agy`) | Google's coding CLI |
| Claude API (`claude-api`) | Anthropic API with your key; for questions and second opinions; cannot edit files |
| OpenAI API (`openai-api`) | OpenAI API with your key; for questions and second opinions; cannot edit files |

Models and effort levels are read live from each CLI or provider and can be switched from the web UI where the CLI allows it.

## Features

- **In the chat:** queue messages while a turn runs, interrupt, rewind, or fork a bot to continue from its context. The raw terminal is one tab away.
- **Team roles (Expert mode):** in a project, one manager bot dispatches work to executor bots and consults advisor bots. Identical workers form a pool, and work goes to whichever is free. Agents a bot starts appear under it, and it is told when they finish or get stuck.
- **Schedules:** send a prompt once, on an interval, daily, or weekly, with a run history.
- **Share and files:** share a bot with someone through a link with restricted access. Files bots produce land in an outbox; HTML files open as web pages and images Antigravity generates show up in the chat. A file shelf passes files between bots.
- **Preview:** see a bot's dev server next to the chat, at phone and iPad sizes.
- **Updates:** one-click updates for herdr and each agent CLI, on every host.
- **Languages:** the UI is in English and Chinese.

Full behavior and limits are in [`docs/SPEC.md`](docs/SPEC.md).

## Platforms and requirements

- **macOS (arm64)** or **Linux (x86_64, arm64)**. On Windows, run it inside **WSL2** and open the UI from a Windows browser.
- **[herdr](https://herdr.dev)**. Homebrew installs it for you; with a prebuilt binary or a source build, install it yourself first (`herdr --version` should work).
- **At least one agent CLI or API key** from the table above.
- Building from source also needs a Rust toolchain and [Bun](https://bun.sh).

## Quick start

```bash
brew install eden-sun/tap/agents-manager    # macOS and Linux; also installs herdr
brew services start agents-manager          # starts now and at every login
```

Open **http://127.0.0.1:7788**. From this machine you are signed in automatically. You should see the AG Man page with an empty sidebar and the quota strip on top.

1. Make sure each agent CLI is logged in: run it once in a terminal, or log in from **Settings → Identities**.
2. Click **Add Project** and pick a directory (on this machine or an SSH host).
3. Open the project's **+** menu, choose **Add Bot**, and pick the agent.
4. Send `hello`. The reply appears as a chat bubble, and the bot shows as running in the sidebar.

**Phone and other devices:** the daemon listens on all network interfaces, so your phone can reach `http://<this machine's address>:7788` on the same LAN or over Tailscale; paste the contents of `~/.config/agents-manager/ui-token` to sign in. Set `AM_DEV_LAN=0` to accept local connections only.

**If your CLIs are not found:** the Homebrew service only sees Homebrew's `bin` and `~/.local/bin`. Stop it (`brew services stop agents-manager`) and run `agents-managerd serve` from your own shell instead. The log is at `$(brew --prefix)/var/log/agents-manager.log`.

<details>
<summary><b>Other ways to install: prebuilt binary or build from source</b></summary>

Both need [herdr](https://herdr.dev) installed first.

**Prebuilt binary** (web UI embedded; `linux-x86_64`, `linux-arm64`, or `macos-arm64`):

```bash
ASSET=linux-x86_64      # or linux-arm64 / macos-arm64
VERSION=$(curl -fsSL https://api.github.com/repos/Eden-Sun/agents-manager/releases/latest | grep -m1 '"tag_name"' | cut -d'"' -f4)
curl -fLO "https://github.com/Eden-Sun/agents-manager/releases/download/$VERSION/agents-managerd-$VERSION-$ASSET.tar.gz"
tar -xzf "agents-managerd-$VERSION-$ASSET.tar.gz"
mkdir -p ~/.local/bin && install -m 755 "agents-managerd-$VERSION-$ASSET/agents-managerd" ~/.local/bin/
~/.local/bin/agents-managerd serve
```

Each archive has a `.sha256` file beside it on the [Releases](https://github.com/Eden-Sun/agents-manager/releases/latest) page. On macOS, a downloaded binary may need `xattr -d com.apple.quarantine ~/.local/bin/agents-managerd` once.

**Build from source:**

```bash
git clone https://github.com/Eden-Sun/agents-manager.git
cd agents-manager
(cd web && bun install && bun run build)        # the web UI is embedded in the daemon
cargo build --release -p agents-managerd
./target/release/agents-managerd serve
```

To keep it running, use a systemd user unit, launchd, or a herdr pane.

</details>

**Just looking?** `git clone` the repo and run `cd web && bun install && VITE_MOCK=1 bun run dev` to open the UI with demo data; no daemon or herdr needed.

## Data and security

- Settings and history live in `~/.config/agents-manager/` (override with `AM_DATA_DIR`). Remote projects keep their data on their own host.
- `ui-token` in that directory grants full control of the UI. Keep it private.
- The packaged macOS app listens only on `127.0.0.1`; `agents-managerd serve` listens on all interfaces unless `AM_DEV_LAN=0`.
- Third-party and outward-facing features (Claude API, OpenAI API, translation, share links) stay off until you turn them on.
- Push notifications go through your browser vendor's push service. Content is encrypted, but the service can see when notifications are sent.

## Development

```bash
scripts/check.sh changed    # verify what changed relative to origin/main
cargo dev                   # daemon on :7788 + Vite dev server on :5173 (cargo down to stop)
```

See [`docs/FRONTEND.md`](docs/FRONTEND.md), [`docs/AGENT-VERIFY.md`](docs/AGENT-VERIFY.md), and [`docs/BUILD-CACHE.md`](docs/BUILD-CACHE.md).

## Documentation

| Document | Contents |
|---|---|
| [`docs/SPEC.md`](docs/SPEC.md) | How everything behaves: bots, hooks, hosts, quota, identities, team roles, schedules |
| [`docs/API.md`](docs/API.md) | HTTP / WebSocket API |
| [`docs/HOOK.md`](docs/HOOK.md) | How each CLI's hooks report replies |
| [`docs/UI-DECISIONS.md`](docs/UI-DECISIONS.md) | Settled interface decisions |
| [`docs/PACKAGING.md`](docs/PACKAGING.md) | Building the macOS `.dmg` |

## License

[MIT](LICENSE)
