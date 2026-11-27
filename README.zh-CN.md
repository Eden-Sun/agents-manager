# AG Man（agents-manager）

[English](README.md) | [繁體中文](README.zh-TW.md) | 简体中文 | [日本語](README.ja.md)

AG Man 是一个运行在你自己机器上的网页界面，用来同时管理多个 coding agent CLI 与 API 模型：**Claude Code（`claude`）、Codex（`codex`）、Grok（`grok`）、Antigravity（`agy`）、Claude API（`claude-api`）与 OpenAI API（`openai-api`）**。每个在 [herdr](https://herdr.dev) pane 里运行的 CLI 会话，都会变成网页上的一个 **bot**。直接使用 API 的 bot 则通过 API key 运行在 daemon 内，不需要 CLI 或终端 pane，专门担任顾问（advisor）。你可以在浏览器或手机上查看对话、回复提问、即时掌握额度与 API 余额，并让多个 bot 相互协作。

AG Man 不是 IDE，也不自己直接启动 CLI agent。终端 pane 由 herdr 管理；daemon（`agents-managerd`）负责与 herdr 通信并通过各 CLI 的 hook 接收回复，或直接与模型提供商 API 通信。

> 截图使用内置的假数据后端（`VITE_MOCK=1`），账号、bot 与消息都是演示数据。

![手机浅色主题：对话、额度芯片与 bot 切换](docs/screenshots/readme/en/mobile-light.png)

## 主要功能

**用对话取代盯终端**
- 每个 CLI 会话或 API 顾问对应一个 bot，按项目目录分组。可在浏览器发送消息、回合执行中排队、中断或强制停止。
- 回复来自 CLI 的 hook、原生 transcript 或直接 API 流。若 daemon 必须退回读取终端快照，该条回复会标示“可能不完整”。
- 确认与多选题会变成可点选的卡片。可以回退对话（rewind）、fork bot 接续上下文，也能唤醒被收起来的闲置 bot。
- 自动检测闲置 bot 的终端草稿，显示“编辑后发送”草稿栏。
- 可随时从设置面板或侧栏“...”菜单重启 bot。

**协调多个 bot**
- **专家模式**（项目层级）：指定一位顶层管理者，其他成员为执行者（executor）、顾问（advisor）、排程者（scheduler）或专家（specialist，特定轨专家）。专家模式下不开子 agent，改由管理者派工给同项目成员。可通过 `manager_use` 单独开关成员以保护额度。
- **群集派工**：同项目内 kind、model、effort、identity 相同的执行者会合并为一个群集，可一次派工或整批开关。
- **API bot 担任顾问**：`claude-api` 与 `openai-api` bot 运行在 daemon 内，通过 API key 运作，不需要终端 pane。严格作为顾问（提供第二意见与背景咨询），支持每回合成本上限（或设为无上限）与即时流式进度。
- **子 agent**：通过 herdr 启动的子 agent 会挂在父 bot 之下；子 agent 完成或卡住时，父 bot 会收到通知。
- **总管（AGM）**：可选的协调者 bot，负责巡检、派工、收起闲置 bot 与部署。见 [SPEC §18](docs/SPEC.md)。

**排程与推送**
- 项目排程：单次、固定间隔、每日、每周，以及多时段；排程 prompt 会送进专属排程 bot（`role=scheduler`），支持快照／干净重启（含 `agy`），并保留执行记录。
- 网页推送（Web Push）：回合完成、需要回应、排程结束、上游新模型上线与额度重置时通知。可在设置中分别开关；若用户已在任一设备上读过该消息、或正在手机前台使用 AG Man，就不再推送。
- 长消息完成时推送简明预览“消息较长，点击查看完整内容”，点击直接打开对话。
- Codex 重置券提示可延后 1 小时、1 天或 3 天（“不再提醒”）。

**远程主机与账号额度**
- 项目可以放在 SSH 主机上，通过该主机的 herdr 运行：远程 bot、主机 shell、CLI 登录、额度，也能把项目移交到另一台 daemon。
- **身份页（Identities）**：浏览各主机上的身份，具备主机分页与各 CLI 卡片（Claude、Codex、Grok、agy）。“添加账号”对话框引导建立隔离设置目录、提示共享风险、支持跨主机复制与同步身份，并在后台辅助完成登录（含自动提取 `agy` 的 Google 授权网址）。默认账号可隐藏或登出。
- **全局 API 余额显示**：顶部额度栏与下拉菜单即时显示各主机的 Anthropic API 与 OpenAI API 余额（OpenAI 直接读取 platform `credit_grants` 实际值），桌面版具备双行胶囊。
- **模型命名统一与排序**：Codex 与 OpenAI API 统一模型命名，模型按家族价格由便宜到贵排序（Luna < Terra < Sol < Astra；Flash < Hydrogen < Argon < Neon），旧版本明确标注。
- 额度与重置时间按主机与身份显示；额度偏低时会警示。
- 模型、effort、fast mode 在 CLI 支持时可直接在网页切换。

**更新与维运**
- “更新检查”页列出每台主机的 herdr、各 agent CLI 与 hook 集成版本，可一键更新。
- 一次 CLI 升级收敛为单一可点击芯片；安装→重启→排程串接由 daemon 在后台执行，关掉网页也照跑。
- 闲置 bot 在设定的时间后会被收起，只保留恢复所需的信息。

**工作区与防护**
- 可在对话旁预览 bot 的本机 dev server，支持切换手机与 iPad 视口尺寸。
- outbox HTML 预览与 `agy` 产图：直接在新标签页完整预览 bot 产出的 `.html` 文件；`agy` 产生的图片自动内嵌进聊天并同步列进 outbox（保留 1 小时）。
- 文件架（跨 bot 暂存）与 outbox（bot 产出的可下载文件，支持鼠标与 iPad Apple Pencil 悬停预览图片）。
- 内存与 CPU 视图：查看各主机整机 CPU% 与 herdr 进程树。
- 内置 hook 会拦截会杀掉其他 agent 的广域 `pkill`／`kill` 命令。

**中英文界面**：默认跟随浏览器语言，可在“设置 → Display”切换。

更多细节与限制见 [`docs/SPEC.md`](docs/SPEC.md)。

**不做的事**：代码编辑器、diff／PR 审查、git worktree 管理（由 agent 按各 repo 惯例自行处理）。

## 支持平台

| 平台 | 状态 |
|---|---|
| macOS（arm64） | 支持 daemon 与网页界面 |
| Linux（x86_64、arm64） | 支持 daemon 与网页界面 |
| Windows | 不支持原生 Windows daemon。请在 **WSL2**（Linux 发行版）内安装与运行，网页从 Windows 浏览器打开即可 |

网页界面可在任何现代浏览器使用，桌面、iPad（竖／横屏）与手机皆可。

## 需求与安装

**必要**（Homebrew 与预先构建的可执行文件方式会自动安装 herdr；其余需自行准备）

| 工具 | 用途 |
|---|---|
| [herdr](https://herdr.dev) | 终端工作区管理器。daemon 通过它的 Unix socket 开 pane、启动 agent、发送文字、读取画面。安装后用 `herdr --version` 确认；设置页的“更新检查”会比对版本。 |
| 至少一个 agent CLI 或 API key | Agent CLI：`claude`、`codex`、`grok` 或 `agy`。API bot：Anthropic API key 或 OpenAI API key。 |
| Rust 工具链（`cargo`） | 只有从源码构建时需要。 |
| [Bun](https://bun.sh) | 只有从源码构建时需要（构建网页前端）。 |

**建议安装**（主机环境检查会列出，缺少时会提示）

| 工具 | 用途 |
|---|---|
| `rtk` | token 压缩代理；Claude 的 hook 会把一般命令改写成它。 |
| `zsh`、`jq` | 主机 shell 与 hook 脚本使用。 |
| `gh` | 显示 GitHub CLI 登录状态。 |

**可选**：Blaze 构建引擎可加快开发构建，见 [`docs/BUILD-CACHE.md`](docs/BUILD-CACHE.md)。Claude API key、OpenAI API key 与 Jev 判断层都是可选功能，默认关闭。

## 快速开始

**方式一：Homebrew（macOS 与 Linux，会一并安装 herdr）**

```bash
brew install eden-sun/tap/agents-manager
brew services start agents-manager    # 停止：brew services stop agents-manager
```

服务会在登录后自动启动并保持运行。若想在前台运行：`agents-managerd serve`（Ctrl-C 停止）。Log 位于 `$(brew --prefix)/var/log/agents-manager.log`。服务只会看到 Homebrew 的 `bin` 与 `~/.local/bin`；若你的 agent CLI 装在其他位置，请改在 shell 中以前台方式运行。

**方式二：预先构建的可执行文件**（网页界面已内嵌）。到 [Releases](https://github.com/Eden-Sun/agents-manager/releases/latest) 下载对应平台的压缩包：`linux-x86_64`、`linux-arm64` 或 `macos-arm64`。

```bash
VERSION=v0.2.0          # 请使用 Releases 页面上最新的版本号
ASSET=linux-x86_64      # 或 linux-arm64 / macos-arm64
curl -fLO "https://github.com/Eden-Sun/agents-manager/releases/download/$VERSION/agents-managerd-$VERSION-$ASSET.tar.gz"
tar -xzf "agents-managerd-$VERSION-$ASSET.tar.gz"
install -m 755 "agents-managerd-$VERSION-$ASSET/agents-managerd" ~/.local/bin/
agents-managerd serve
```

每个压缩包旁都有 `.sha256` 文件可供核对。macOS 下载的可执行文件可能需要先执行一次 `xattr -d com.apple.quarantine ~/.local/bin/agents-managerd`。压缩包不包含 `scripts/` 下的脚本（例如 `agm`），需要的话请 clone 源码。

**方式三：从源码构建**（开发，或平台不在上面的列表中）。以下以 clone 为例：

```bash
git clone https://github.com/Eden-Sun/agents-manager.git
cd agents-manager

# 1. 构建网页前端（会被嵌入 daemon）
(cd web && bun install && bun run build)

# 2. 构建 daemon
cargo build --release -p agents-managerd

# 3. 启动（前台运行，Ctrl-C 停止）
./target/release/agents-managerd serve
```

无论用哪一种方式，都在浏览器打开 **http://127.0.0.1:7788**。从 `127.0.0.1` 打开会自动登录；其他设备请粘贴 `~/.config/agents-manager/ui-token` 的内容。

想先看界面、不需要 herdr 或 daemon，可以使用假数据后端：

```bash
cd web && bun install && VITE_MOCK=1 bun run dev
```

若要长期运行，Homebrew 的 `brew services` 会代为管理；其他方式可以使用 systemd user unit、launchd，或放在一个 herdr pane 里。Linux 主机的排程细节见 [SPEC §18.2e](docs/SPEC.md)。

## 第一次使用

1. **新增项目**：在侧栏底部点“新增 Project”，选择本机或 SSH 主机上的目录，并取一个标签。
2. **新增 bot**：打开项目的“＋”菜单，选“新增 Bot”，选择 kind（`claude`、`codex`、`grok`、`agy`、`claude-api` 或 `openai-api`），可选择身份与模型。daemon 会开一个 herdr pane 启动 CLI，或建立直连 API 顾问。
3. **对话**：在输入框发送消息。回合执行中发送的消息会排队；确认与多选题以卡片显示；停止按钮可中断回合。需要看原始画面时切到“终端”标签页。
4. **让 bot 相互委派**：bot 若执行 `herdr agent start …`，子 agent 会列在它之下，并在完成或卡住时通知它。专家模式下，管理者直接派工给项目内的执行者与顾问。
5. **自动化**：同一个“＋”菜单有“新增排程”。账号额度、模型列表、更新检查与主机都在“环境设置”（Settings）里。
6. **用手机打开**：在同一网络（LAN 或 Tailscale）上，用 `http://<本机地址>:7788` 打开，并粘贴 `ui-token`。

## 支持的 kind 与模型

AG Man 支持两类 bot：
- **CLI bot**（`claude`、`codex`、`grok`、`agy`）：运行在 [herdr](https://herdr.dev) 终端 pane 内并注入 hook。可执行 shell 命令、编辑文件并完整运行 agent 循环。
- **API bot**（`claude-api`、`openai-api`）：由 daemon 通过 API key 直接调用模型提供商 API，不需要 CLI 或终端 pane。严格作为**顾问（Advisor）**运行（提供咨询与第二意见），支持每回合成本上限（或无上限）。

> **注意**：daemon 会从各 CLI 与 API 提供商动态获取最新模型列表。下表为发布／撰写时的快照（2026-11-27）。

| kind | 类型 | 说明 | 当前模型（按家族／等级） | effort 范围 |
|---|---|---|---|---|
| `claude` | CLI | 运行在 herdr pane 的 Anthropic Claude Code | `opus`、`sonnet`（默认）、`haiku`、`fable` | `low`、`medium`、`high`、`xhigh`、`max` |
| `codex` | CLI | 运行在 herdr pane 的 OpenAI Codex CLI | `gpt-6-luna` < `gpt-6.1-terra` < `gpt-6.1-sol` < `gpt-6.2-sol`（默认） < `gpt-6-astra` < `gpt-6.2-astra` | `low`、`medium`、`high`、`xhigh`、`max`、`ultra`（luna 上限为 `max`） |
| `grok` | CLI | 运行在 herdr pane 的 xAI Grok CLI | `grok-4.7`（默认） | `low`、`medium`、`high`、`xhigh` |
| `agy` | CLI | 运行在 herdr pane 的 Google Antigravity | 依账号：默认账号 ag0、其他身份 ag1…；各账号可用模型不同（例 ag0：Gemini 3.8 Flash＋Gemini 4 Neon／Argon／Hydrogen；ag1：只有 3.8 Flash），模型菜单依主机＋账号列出；context 视窗 1,048,576；有 1 小时 cache 倒计时、不保温 | 预设嵌在模型名称内；闲置时可即时切换 `/model` 与 `/effort` |
| `claude-api` | API | 直连 Anthropic API（仅限 Advisor） | `claude-opus-5-5`（默认）、`claude-fable-5-2` | `low`、`medium`、`high`、`xhigh`、`max` |
| `openai-api` | API | 直连 OpenAI API（仅限 Advisor） | `gpt-6-luna` < `gpt-6.2-sol` < `gpt-6.2-astra`（默认） | `none`（luna）、`low`、`medium`、`high`、`xhigh`、`max` |

### 如何添加 Bot
- **网页界面**：在项目的“＋”菜单中选择**新增 Bot**，选择 kind、模型与 effort 等级（在专家模式下可选择角色 role；API bot 只能为顾问 advisor）。
- **CLI（专家模式）**：
  ```bash
  agm hire <名称> --kind <kind> --model <model> --effort <effort> [--role <role>] [--track <track>] [--identity <identity>]
  ```
  *（注：`agm hire` 只能聘执行者 `executor` [默认] 或专家 `specialist` [须带 `--track`]；顾问由用户在网页端创建。执行者命名遵循规则 `<账号>-<模型简写>-<序号>`。示例：`agm hire cc0-hk-1 --kind claude --model haiku --effort medium`、`agm hire ag0-ne-1 --kind agy --model gemini-4-neon-medium`，或指定身份 `agm hire cc1-hk-1 --kind claude --model haiku --effort medium --identity cc1`）*

### 支持的 agent CLI 与 hook 机制

每个 CLI 在启动时会被注入 hook，因此回复是结构化事件，而不是从画面抓取文字。

| kind | 回复来源 | 备注 |
|---|---|---|
| `claude` | `--settings` 的 hook（`SessionStart`／`Stop`／`StopFailure`） | 多身份；模型与 effort 可即时切换 |
| `codex` | Stdin `Stop` hook 与 `notify` hook | 模型、effort、Fast 可即时切换 |
| `grok` | `GROK_HOME` 内的 hook 文件 | 模型可即时切换；effort 需重启；见 [SPEC §12](docs/SPEC.md) |
| `agy` | Antigravity CLI 设置中的具名 hook | 标题栏显示更新提示；多身份（ag0、ag1…）；闲置时即时切换 `/model` 与 `/effort`；产图自动收进聊天与 outbox；context 视窗 1,048,576；1 小时 cache 倒计时、不保温；见 [SPEC §12a](docs/SPEC.md) |
| `claude-api` | 直连 Anthropic API 流（SSE） | 仅限 Advisor；每回合成本上限、prompt cache 倒计时 |
| `openai-api` | 直连 OpenAI Responses API 流（SSE） | 仅限 Advisor；每回合成本上限、reasoning tokens 显示 |

hook 细节见 [`docs/HOOK.md`](docs/HOOK.md)。

## 设置与数据位置

默认位置：`~/.config/agents-manager/`（可用环境变量 `AM_DATA_DIR` 覆盖）。

| 文件 | 内容 |
|---|---|
| `config.toml` | 期望状态：项目、bot、主机、设置。是事实来源。 |
| SQLite 数据库 | 运行时状态：回合、消息、排程记录等。 |
| `ui-token` | 网页登录用的 token，权限为 0600。 |

设置示例（数值与路径仅为示意）：

```toml
[server]
listen = "127.0.0.1:7788"
herdr_session = "agents-manager"

[[projects]]
id = "01EXAMPLE000000000000000000"
path = "/home/example/project/foo"
label = "foo"

  [[projects.bots]]
  id = "01EXAMPLE000000000000000001"
  name = "foo-claude"
  kind = "claude"
```

- 数据跟着设置文件走：用 `--config` 指向其他路径时，数据库与 `ui-token` 会跟着该目录（或 `[server] data_dir`），两者必须一致。
- 写回 `config.toml` 时会保留你的注释与排版。
- 设置页（Settings）的分页：Setup、Hosts、Update check、Identities、Jev suggestions、Claude API、OpenAI API、Display。

完整字段见 [SPEC §5](docs/SPEC.md)。

## 安全注意

- **网络绑定**：打包好的 macOS app 只绑定 `127.0.0.1`。其他情况下（包括直接运行 `agents-managerd serve`），daemon 默认绑定所有接口，让同一网络的手机也能连接。若只想供本机使用，请设置 `AM_DEV_LAN=0`。
- **ui-token**：持有它就等于拥有网页的完整权限，包括启动与停止 bot。请勿提交到 git，也请勿贴到公开场合。共用 token 的 LAN 风险是有意保留的设计取舍，见 [API.md §0](docs/API.md)。
- **数据只在本机**：对话、设置与数据库都存放在你的机器上。远程主机的项目则通过 SSH 访问该主机上的数据。
- **第三方服务默认关闭**：Claude API（一次性 LLM 任务、changelog 分诊、消息翻译）、OpenAI API（顾问、翻译）、Jev 判断层（外部 API key）与分享入口都需要你明确开启。翻译按钮只有点击时才会产生费用。简体转繁体在本机完成，不调用 API。
- **网页推送**：通知经由浏览器厂商的推送服务送达，内容加密，但推送服务可以看到推送的时间与对象。可在设置中关闭。
- **分享 bot**：分享链接可让受限或受信任的 bot 被其他人使用，属于独立的入口，见 [SPEC §20](docs/SPEC.md)。
- **广域 kill 防护**：PreToolUse hook（claude、本机 grok）与 `pkill`／`killall` 的 PATH shim 会拦截可能杀掉其他 agent 的命令：无目标的 `pkill`／`killall`、匹配 agent 进程名称的 pattern、BSD `pkill` 会误读的选项顺序、`kill -1`／`kill 0`。明确指定 PID 与 `lsof -ti :PORT | xargs kill` 则放行。请不要为了绕过它而改用别的写法。各 CLI 的防护覆盖率见 SPEC §4.4c。

## 开发与验证

```bash
scripts/check.sh changed    # 只验证相对 origin/main 有改动的部分（提交前运行）
scripts/check.sh web        # TypeScript、lint、前端测试与构建
scripts/check.sh daemon     # daemon、xtask 与 crates 的 cargo 测试
scripts/check.sh ops        # shell 脚本的 lint 与隔离测试
```

- 开发模式：`cargo dev` 同时启动 daemon（`0.0.0.0:7788`）与 Vite dev server（`0.0.0.0:5173`），`cargo down` 停止。
- 前端的命令与惯例见 [`docs/FRONTEND.md`](docs/FRONTEND.md) 与 [`web/README.md`](web/README.md)。
- 验证与 CI 流程见 [`docs/AGENT-VERIFY.md`](docs/AGENT-VERIFY.md)。

## 文档索引

| 文档 | 内容 |
|---|---|
| [`docs/SPEC.md`](docs/SPEC.md) | 当前行为的规格：数据模型、hook、对账、远程主机、额度、身份、总管、群集、排程 |
| [`docs/API.md`](docs/API.md) | HTTP／WebSocket 契约，包含 bot 与服务身份的授权范围 |
| [`docs/UI-DECISIONS.md`](docs/UI-DECISIONS.md) | 已定案的界面取舍 |
| [`docs/FRONTEND.md`](docs/FRONTEND.md) | 前端结构、命令与惯例 |
| [`docs/HOOK.md`](docs/HOOK.md) | hook 子命令的契约与计时测试 |
| [`docs/AGENT-VERIFY.md`](docs/AGENT-VERIFY.md) | 验证与 CI 工作流程 |
| [`docs/BUILD-CACHE.md`](docs/BUILD-CACHE.md) | Blaze 开发构建与 sccache 依赖缓存 |
| [`docs/PACKAGING.md`](docs/PACKAGING.md) | 打包 macOS `.dmg` |
| [`docs/CLAUDE-NATIVE-TRANSPORT.md`](docs/CLAUDE-NATIVE-TRANSPORT.md) | Claude 原生传输与 herdr 的比较评估 |
| [`docs/CHATGPT-CONSULT.md`](docs/CHATGPT-CONSULT.md) | 可选的 ChatGPT 网页咨询 worker |
| [`web/README.md`](web/README.md) | 前端快速命令 |

## 授权

[MIT](LICENSE)
