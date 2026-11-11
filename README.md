# AG Man（agents-manager）

繁體中文 · [English summary](#english-summary)

AG Man 是一個跑在你自己機器上的網頁介面，用來管理多個 coding agent CLI：**Claude Code（`claude`）、Codex（`codex`）、Grok（`grok`）與 Antigravity（`agy`）**。每個在 [herdr](https://herdr.dev) pane 裡跑的 CLI 工作階段，都會變成網頁上的一個 **bot**：你可以在瀏覽器或手機上看對話、回覆提問、看額度，並讓多個 bot 互相協作。

AG Man 不是 IDE，也不自己啟動 agent。終端機由 herdr 管理；daemon（`agents-managerd`）負責和 herdr 溝通，並透過各 CLI 的 hook 接收回覆。

> 截圖使用內建的假資料後端（`VITE_MOCK=1`），帳號、bot 與訊息都是示範資料。

![手機淺色主題：對話、額度晶片與 bot 切換](docs/screenshots/readme/en/mobile-light.png)

## 主要功能

**用對話取代盯終端機**
- 每個 CLI 工作階段對應一個 bot，依專案目錄分組。可在瀏覽器送訊息、回合執行中排隊、中斷或強制停止。
- 回覆來自 CLI 的 hook 或原生 transcript。若 daemon 必須退回讀取終端快照，該則回覆會標示「可能不完整」。
- 確認與多選題會變成可點選的卡片。可以倒回對話（rewind）、fork bot 接續脈絡，也能喚醒閒置被收起來的 bot。

**協調多個 bot**
- **專家模式**（專案層級）：指定一位頂層管理者，其他成員為執行者（executor）、顧問（advisor）、排程者（scheduler）或專家（specialist）。專家模式下不開子 agent，改由管理者派工給同專案成員。
- **群集派工**：同專案內 kind、model、effort、identity 相同的執行者會合併成一個群集，可一次派給群集。
- **子 agent**：透過 herdr 啟動的子 agent 會掛在父 bot 底下；子 agent 完成或卡住時，父 bot 會收到通知。
- **總管（AGM）**：選用的協調者 bot，負責巡檢、派工、收起閒置 bot 與部署。見 [SPEC §18](docs/SPEC.md)。

**排程與推播**
- 專案排程：單次、固定間隔、每日、每週，以及多時段；排程會送進專屬的排程 bot，並保留執行紀錄。
- 網頁推播（Web Push）：回合完成、需要回應、排程終態、有新版或額度重置時通知。可在設定中分別開關；若使用者已在任一裝置讀過該訊息，就不再推播。

**遠端主機與帳號額度**
- 專案可以放在 SSH 主機上，透過該主機的 herdr 運作：遠端 bot、主機 shell、CLI 登入、額度，也能把專案移交到另一台 daemon。
- Claude 身份 `cc0`–`cc6` 會從 shell alias 自動探索（每台主機各一份），可一鍵登入或停用。
- 額度與重置時間依主機與身份顯示；低額度會警示。提供的資訊依各家 CLI 而定（Claude、Codex、Grok 較完整；agy 只顯示 Gemini 額度）。
- 模型、effort、fast mode 在 CLI 支援時可直接在網頁切換。

**更新與維運**
- 「更新檢查」頁列出每台主機的 herdr、各 agent CLI 與 hook 整合版本，可一鍵更新。
- 閒置 bot 在設定的時間後會被收起，只保留恢復所需的資訊。

**工作區與防護**
- 可在對話旁預覽 bot 的本機 dev server。
- 檔案架（跨 bot 暫存）與 outbox（bot 產出的可下載檔案）。
- 記憶體檢視：查看 herdr 行程樹。
- 內建 hook 會攔截會殺掉其他 agent 的廣域 `pkill`／`kill` 指令。

**中英文介面**：預設跟隨瀏覽器語言，可在「設定 → Display」切換。

更多細節與限制見 [`docs/SPEC.md`](docs/SPEC.md)。

**不做的事**：程式碼編輯器、diff／PR 審查、git worktree 管理（由 agent 依各 repo 慣例自行處理）。

## 支援平台

| 平台 | 狀態 |
|---|---|
| macOS（arm64） | 支援 daemon 與網頁介面 |
| Linux（x86_64、arm64） | 支援 daemon 與網頁介面 |
| Windows | 不支援原生 Windows daemon。請在 **WSL2**（Linux 發行版）內安裝與執行，網頁從 Windows 瀏覽器開啟即可 |

網頁介面可在任何現代瀏覽器使用，桌機與手機皆可。

## 需求與安裝

**必要**

| 工具 | 用途 |
|---|---|
| [herdr](https://herdr.dev) | 終端工作區管理器。daemon 透過它的 Unix socket 開 pane、啟動 agent、送字、讀畫面。安裝後用 `herdr --version` 確認；設定頁的「更新檢查」會比對版本。 |
| 至少一個 agent CLI：`claude`、`codex`、`grok` 或 `agy` | 你要管理的 agent。請先在本機登入一次。 |
| Rust 工具鏈（`cargo`） | 從原始碼建置 daemon。 |
| [Bun](https://bun.sh) | 建置網頁前端。 |

**建議安裝**（主機環境檢查會列出，缺少時會提示）

| 工具 | 用途 |
|---|---|
| `rtk` | token 壓縮代理；Claude 的 hook 會把一般指令改寫成它。 |
| `zsh`、`jq` | 主機 shell 與 hook 腳本使用。 |
| `gh` | 顯示 GitHub CLI 登入狀態。 |

**選用**：Blaze 建置引擎可加快開發建置，見 [`docs/BUILD-CACHE.md`](docs/BUILD-CACHE.md)。Claude API key 與 Jev 判斷層都是選用功能，預設關閉。

## 快速開始

以下以原始碼建置為例。把 `<repo-url>` 換成你的 clone 網址：

```bash
git clone <repo-url> agents-manager
cd agents-manager

# 1. 建置網頁前端（會被嵌入 daemon）
(cd web && bun install && bun run build)

# 2. 建置 daemon
cargo build --release -p agents-managerd

# 3. 啟動（前景執行，Ctrl-C 停止）
./target/release/agents-managerd serve
```

然後在瀏覽器開啟 **http://127.0.0.1:7788**。從 `127.0.0.1` 開啟會自動登入；其他裝置請貼上 `~/.config/agents-manager/ui-token` 的內容。

想先看介面、不需要 herdr 或 daemon，可以用假資料後端：

```bash
cd web && bun install && VITE_MOCK=1 bun run dev
```

若要長期執行，可以用 systemd user unit、launchd，或放在一個 herdr pane 裡。Linux 主機的排程細節見 [SPEC §18.2e](docs/SPEC.md)。

## 第一次使用

1. **新增專案**：在側欄底部按「新增 Project」，選擇本機或 SSH 主機上的目錄，並取一個標籤。
2. **新增 bot**：打開專案的「＋」選單，選「新增 Bot」，選擇 kind（`claude`、`codex`、`grok` 或 `agy`），可選擇身份與模型。daemon 會開一個 herdr pane，在裡面啟動 CLI 並掛上 hook。
3. **對話**：在輸入框送出訊息。回合執行中送出的訊息會排隊；確認與多選題會以卡片顯示；停止按鈕可中斷回合。需要看原始畫面時切到「終端」分頁。
4. **讓 bot 互相委派**：bot 若執行 `herdr agent start …`，子 agent 會列在它底下，並在完成或卡住時通知它。
5. **自動化**：同一個「＋」選單有「新增排程」。帳號額度、模型清單、更新檢查與主機都在「環境設定」（Settings）裡。
6. **用手機開啟**：在同一個網路（LAN 或 Tailscale）上，用 `http://<本機位址>:7788` 開啟，並貼上 `ui-token`。

## 支援的 agent CLI

每個 CLI 在啟動時會被注入 hook，因此回覆是結構化事件，而不是從畫面抓文字。

| kind | 回覆來源 | 備註 |
|---|---|---|
| `claude` | `--settings` 的 hook（`SessionStart`／`Stop`） | 身份 `cc0`–`cc6`；模型與 effort 可即時切換 |
| `codex` | 每次啟動的 `notify` hook | 模型、effort、Fast 可即時切換 |
| `grok` | `GROK_HOME` 內的 hook 檔 | 模型可即時切換；effort 需重啟，見 [SPEC §12](docs/SPEC.md) |
| `agy` | Antigravity CLI 設定中的具名 hook | 更新提示顯示在標題列；見 [SPEC §12a](docs/SPEC.md) |

hook 細節見 [`docs/HOOK.md`](docs/HOOK.md)。

## 設定與資料位置

預設位置：`~/.config/agents-manager/`（可用環境變數 `AM_DATA_DIR` 覆寫）。

| 檔案 | 內容 |
|---|---|
| `config.toml` | 期望狀態：專案、bot、主機、設定。是事實來源。 |
| SQLite 資料庫 | 執行期狀態：回合、訊息、排程紀錄等。 |
| `ui-token` | 網頁登入用的 token，權限為 0600。 |

設定範例（數值與路徑皆為示意）：

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

- 設定檔在哪，資料就在哪：用 `--config` 指向其他路徑時，資料庫與 `ui-token` 會跟著該目錄（或 `[server] data_dir`），兩者必須一致。
- 寫回 `config.toml` 時會保留你的註解與排版。
- 設定頁（Settings）的分頁：Setup、Hosts、Update check、Identities、Recently deleted、Jev suggestions、Claude API、Display。

完整欄位見 [SPEC §5](docs/SPEC.md)。

## 安全注意

- **網路綁定**：打包好的 macOS app 只綁 `127.0.0.1`。其他情況下（包含直接執行 `agents-managerd serve`），daemon 預設會綁定所有介面，讓同一網路的手機也能連上。若只想給本機用，請設定 `AM_DEV_LAN=0`。
- **ui-token**：擁有它就等於擁有完整的網頁權限，包括啟動與停止 bot。請勿提交到 git、請勿貼到公開場合。共用 token 的 LAN 風險是有意保留的設計取捨，見 [API.md §0](docs/API.md)。
- **資料只在本機**：對話、設定與資料庫都存在你的機器上。遠端主機的專案則透過 SSH 存取該主機上的資料。
- **第三方服務預設關閉**：Claude API（一次性 LLM 工作、changelog 分診、訊息翻譯）、Jev 判斷層（外部 API key）與分享入口都需要你明確開啟。翻譯按鈕是點擊才會扣費。簡體中文轉繁體則在本機完成，不送 API。
- **網頁推播**：推播內容會經過瀏覽器的推播服務（由各瀏覽器供應商提供）送達，內容加密，但推播服務會知道推送的時間與對象。可在設定中關閉。
- **分享 bot**：分享連結可讓受限或受信任的 bot 被其他人使用，屬於獨立的入口，見 [SPEC §20](docs/SPEC.md)。
- **廣域 kill 防護**：hook 會攔截可能殺掉其他 agent 的 `pkill`／`kill` 指令。請不要為了繞過它而改用別的寫法。

## 開發與驗證

```bash
scripts/check.sh changed    # 只驗證與 origin/main 相比有改到的部分（提交前執行）
scripts/check.sh web        # TypeScript、lint、前端測試與建置
scripts/check.sh daemon     # daemon、xtask 與 crates 的 cargo 測試
scripts/check.sh ops        # shell 腳本的 lint 與隔離測試
```

- 開發模式：`cargo dev` 同時啟動 daemon（`0.0.0.0:7788`）與 Vite dev server（`0.0.0.0:5173`），`cargo down` 停止。
- 前端的指令與慣例見 [`docs/FRONTEND.md`](docs/FRONTEND.md) 與 [`web/README.md`](web/README.md)。
- 驗證與 CI 流程見 [`docs/AGENT-VERIFY.md`](docs/AGENT-VERIFY.md)。

## 文件索引

| 文件 | 內容 |
|---|---|
| [`docs/SPEC.md`](docs/SPEC.md) | 目前行為的規格：資料模型、hook、對帳、遠端主機、額度、身份、總管、群集、排程 |
| [`docs/API.md`](docs/API.md) | HTTP／WebSocket 契約，包含 bot 與服務身份的授權範圍 |
| [`docs/UI-DECISIONS.md`](docs/UI-DECISIONS.md) | 已定案的介面取捨 |
| [`docs/FRONTEND.md`](docs/FRONTEND.md) | 前端結構、指令與慣例 |
| [`docs/HOOK.md`](docs/HOOK.md) | hook 子命令的契約與計時測試 |
| [`docs/AGENT-VERIFY.md`](docs/AGENT-VERIFY.md) | 驗證與 CI 工作流程 |
| [`docs/BUILD-CACHE.md`](docs/BUILD-CACHE.md) | Blaze 開發建置與 sccache 快取 |
| [`docs/PACKAGING.md`](docs/PACKAGING.md) | 打包 macOS `.dmg` |
| [`docs/CLAUDE-NATIVE-TRANSPORT.md`](docs/CLAUDE-NATIVE-TRANSPORT.md) | Claude 原生傳輸與 herdr 的比較評估 |
| [`docs/CHATGPT-CONSULT.md`](docs/CHATGPT-CONSULT.md) | 選用的 ChatGPT 網頁諮詢 worker |
| [`web/README.md`](web/README.md) | 前端快速指令 |

## English summary

AG Man (agents-manager) is a self-hosted web UI for managing several coding-agent CLIs (Claude Code, Codex, Grok and Antigravity `agy`) running in [herdr](https://herdr.dev) panes. Each CLI session becomes a "bot": you can read conversations, answer prompts, watch quota, run expert-mode teams (manager / executor / advisor / scheduler / specialist), schedule prompts, receive Web Push notifications and work with bots on SSH hosts. The daemon (`agents-managerd`, Rust) talks to herdr and receives replies through each CLI's hooks; the web UI is React.

- **Platforms**: macOS (arm64) and Linux (x86_64, arm64) for the daemon. On Windows, run it inside WSL2. Native Windows is not supported.
- **Requirements**: herdr, at least one logged-in agent CLI, Rust and Bun for building from source.
- **Quick start**: build the web UI (`cd web && bun install && bun run build`), build the daemon (`cargo build --release -p agents-managerd`), run `./target/release/agents-managerd serve`, then open http://127.0.0.1:7788.
- **Data**: `~/.config/agents-manager/` (`config.toml`, SQLite, `ui-token`); override with `AM_DATA_DIR`.
- **Security**: keep `ui-token` private. The packaged macOS app binds loopback only; other runs bind all interfaces unless `AM_DEV_LAN=0`. Third-party services (Claude API, Jev judge, sharing) are off by default.
- **Verify**: `scripts/check.sh changed` before pushing. Full docs are in `docs/`, starting with `docs/SPEC.md`.

## 授權

[MIT](LICENSE)
