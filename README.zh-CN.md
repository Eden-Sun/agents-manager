# AG Man（agents-manager）

[English](README.md) | [繁體中文](README.zh-TW.md) | 简体中文 | [日本語](README.ja.md)

**一个网页，同时指挥整支 coding agent 团队——Claude Code、Codex、Grok、Antigravity 与 API 顾问——运行在你自己的机器上，桌面、iPad、手机都能随时操作。**

同时开多个 coding agent CLI，往往要在一堆终端标签页间切换、漏掉卡在 yes/no 确认题一小时的那一颗，还得猜各账号剩多少额度。AG Man 把所有 agent 会话收进同一个自托管网页界面，需要你介入时立刻通知，还能让多颗 bot 相互协作。

<table>
  <tr>
    <td width="76%"><img src="docs/screenshots/readme/en/desktop-light.png" alt="桌面画面：左侧项目与 bot 列表、中间对话区、顶部额度栏、右侧 outbox" /></td>
    <td width="24%"><img src="docs/screenshots/readme/en/mobile-light.png" alt="手机画面：同步对话、额度芯片与 bot 切换" /></td>
  </tr>
</table>

> 截图使用内置假数据后端（`VITE_MOCK=1`）；账号、bot 与消息皆为演示数据。

## 运作方式

AG Man 由一个轻量本机 daemon（`agents-managerd`）与内嵌网页界面组成。CLI agent 运行在 [herdr](https://herdr.dev)（类似 tmux 的终端工作区管理器）的 pane 里；daemon 负责启动、发送消息，并通过各 CLI 原生 hook 接收回复——拿到的是干净的结构化消息与可点击的确认卡片，而不是从终端画面抓字。API 顾问则直接用 API key 在 daemon 内运行，无需终端。所有数据都留在你自己的机器上。

AG Man 不是 IDE：不提供代码编辑器、diff 审阅或 git worktree 管理，这些交由 agent 按各 repo 惯例自行处理。

## 支持的 agent

| kind | 说明 |
|---|---|
| `claude` | Anthropic Claude Code CLI |
| `codex` | OpenAI Codex CLI |
| `grok` | xAI Grok CLI |
| `agy` | Google Antigravity CLI |
| `claude-api` | 直连 Anthropic API（无需 CLI；担任顾问） |
| `openai-api` | 直连 OpenAI API（无需 CLI；担任顾问） |

CLI bot 可执行命令与编辑文件；API bot 专责问答与第二意见，可设每回合成本上限。可用模型与 effort 等级会从各 CLI 或提供商实时读取，CLI 支持时可直接在网页切换。每种 CLI 均支持多账号。

## 主要功能

**用对话取代盯终端**
- 每个 CLI 会话都变成按项目分组的聊天室，状态一目了然（执行中、完成、等你回应）。
- 回合执行中可直接排队下一条消息；确认与多选题自动转为可点击卡片。
- 随时中断、回退（rewind）、fork 接续上下文，或一键切回原始终端标签页。

**任何设备都掌握进度**
- 支持桌面、iPad（竖／横屏，支持 Apple Pencil 悬停预览）与手机，局域网或 Tailscale 皆可连。
- 网页推送（Web Push）通知回合完成、等待回答、排程结果与额度重置；你正在看画面时自动静音。

**让多颗 bot 组队协作**
- **团队角色（专家模式）**：为项目指派管理者 bot，由它派工给执行者并咨询顾问。
- **群集派工**：将同规格执行者组成群集，自动分配给闲置成员。
- **子 agent 与总管**：通过 herdr 开启的子 agent 会缩进挂在父 bot 下，完成或卡住时自动回报；另可选配总管 bot 巡检并收起闲置 bot。

**账号、额度与远程主机**
- 顶部单一额度栏实时显示各账号、各主机的订阅额度、重置倒计时与 Anthropic／OpenAI API 余额。
- 在“身份（Identities）”页添加与登录隔离账号，并支持跨已连接主机同步身份。
- 项目可放在远程 SSH 主机，享有相同的 bot、shell、登录与额度视图。

**自动化与主机健康**
- 项目排程（单次、间隔、每日、每周、多时段）并保留执行记录。
- 单键更新每台主机上的 herdr、各 agent CLI 与 hook。
- 闲置 bot 自动休眠并保留恢复上下文；侧栏实时显示各主机内存与 CPU 使用率。
- 内置防护拦截广域 `pkill`/`killall`，避免误杀其他 agent。

**工作区增值工具**
- 对话旁直接预览 bot 的本机 dev server，支持切换手机与 iPad 尺寸。
- 跨 bot **文件暂存**与每颗 bot 的 **Outbox**，支持 `.html` 一键网页预览与产图内嵌聊天。
- **分享链接**：通过独立入口将受限或受信任的 bot 分享给团队成员使用。

提供中英文界面（于“设置 → Display”切换）。完整行为与限制见 [`docs/SPEC.md`](docs/SPEC.md)。

## 支持平台与需求

- **macOS（arm64）**与 **Linux（x86_64、arm64）**。Windows 请在 **WSL2** 内运行，由 Windows 浏览器打开网页。
- **[herdr](https://herdr.dev)**——Homebrew 会自动安装；若使用预编译可执行文件或从源码构建，请先从 [herdr.dev](https://herdr.dev) 安装 `herdr`。
- **至少一个上表支持的 agent CLI 或 API key**。
- 从源码构建另需 Rust 工具链（`cargo`）与 [Bun](https://bun.sh)。

设置页会自动检查各主机环境，缺少可选辅助工具（`rtk`、`zsh`、`jq`、`gh`）时主动提示。

## 快速开始

> **想先看界面、不装 daemon 或 herdr：**
> ```bash
> git clone https://github.com/Eden-Sun/agents-manager.git
> cd agents-manager/web && bun install && VITE_MOCK=1 bun run dev
> ```

**方式一：Homebrew**（macOS 与 Linux，会自动安装 `herdr`）

```bash
brew install eden-sun/tap/agents-manager
brew services start agents-manager    # 停止：brew services stop agents-manager
```

服务会在登录时自动启动，但只看得到 Homebrew 的 `bin` 与 `~/.local/bin`；若你的 agent CLI 装在其他位置，请改在 shell 运行 `agents-managerd serve`。

**方式二：预编译可执行文件**（已内嵌网页界面；release 归档仅含 `agents-managerd`、不含 `herdr`，请先从 [herdr.dev](https://herdr.dev) 安装 `herdr` 并确认在 `PATH` 中）。从 [Releases](https://github.com/Eden-Sun/agents-manager/releases/latest) 下载最新版：

```bash
VERSION=v0.2.0          # 请改为 Releases 页最新版本号
ASSET=linux-x86_64      # 或 linux-arm64 / macos-arm64
curl -fLO "https://github.com/Eden-Sun/agents-manager/releases/download/$VERSION/agents-managerd-$VERSION-$ASSET.tar.gz"
tar -xzf "agents-managerd-$VERSION-$ASSET.tar.gz"
install -m 755 "agents-managerd-$VERSION-$ASSET/agents-managerd" ~/.local/bin/
agents-managerd serve   # 请确认 ~/.local/bin 在 PATH 中
```

macOS 下载的可执行文件首次运行可能需要 `xattr -d com.apple.quarantine ~/.local/bin/agents-managerd`。

**方式三：从源码构建**（需先安装 `herdr`、Rust 与 Bun）

```bash
git clone https://github.com/Eden-Sun/agents-manager.git
cd agents-manager
(cd web && bun install && bun run build)        # 构建网页并内嵌至 daemon
cargo build --release -p agents-managerd
./target/release/agents-managerd serve
```

启动后打开 **http://127.0.0.1:7788**（本机自动登录；手机或其他设备请打开 `http://<主机 IP>:7788` 并粘贴 `~/.config/agents-manager/ui-token`）。点击 **新增 Project**、再点击 **新增 Bot** 即可开始对话。

## 数据与安全

- 设置与历史记录存于 `~/.config/agents-manager/`（可用 `AM_DATA_DIR` 覆盖）；远程项目数据留在各自远程主机。
- 该目录内的 `ui-token` 拥有完整网页权限，请妥善保管。
- 直接运行 `agents-managerd serve` 默认监听所有接口以供局域网／Tailscale 手机连接；若只限本机可设 `AM_DEV_LAN=0`。打包版 macOS app 仅监听 `127.0.0.1`。
- 付费第三方功能（Claude API、OpenAI API、消息翻译、分享入口）默认全部关闭，需手动启用。
- 网页推送经由各浏览器厂商推送服务传递（内容加密，但推送服务可见发送时间与对象）。

## 开发

```bash
scripts/check.sh changed    # 只验证相对于 origin/main 的改动
cargo dev                   # 同时启动 daemon (:7788) 与 Vite (:5173)，cargo down 停止
```

详见 [`docs/FRONTEND.md`](docs/FRONTEND.md)、[`docs/AGENT-VERIFY.md`](docs/AGENT-VERIFY.md) 与 [`docs/BUILD-CACHE.md`](docs/BUILD-CACHE.md)。

## 文档索引

| 文档 | 内容 |
|---|---|
| [`docs/SPEC.md`](docs/SPEC.md) | 完整行为规格：bot、hook、主机、额度、身份、专家模式、排程 |
| [`docs/API.md`](docs/API.md) | HTTP／WebSocket API |
| [`docs/HOOK.md`](docs/HOOK.md) | 各 CLI hook 回报机制 |
| [`docs/UI-DECISIONS.md`](docs/UI-DECISIONS.md) | 已定案的界面取舍 |
| [`docs/PACKAGING.md`](docs/PACKAGING.md) | 打包 macOS `.dmg` |

## 许可

[MIT](LICENSE)
