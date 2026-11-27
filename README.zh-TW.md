# AG Man（agents-manager）

[English](README.md) | 繁體中文 | [简体中文](README.zh-CN.md) | [日本語](README.ja.md)

AG Man 是一個跑在你自己機器上的網頁介面，用來同時管理多個 coding agent CLI 與 API 模型：**Claude Code（`claude`）、Codex（`codex`）、Grok（`grok`）、Antigravity（`agy`）、Claude API（`claude-api`）與 OpenAI API（`openai-api`）**。每個在 [herdr](https://herdr.dev) pane 裡跑的 CLI 工作階段，都會變成網頁上的一個 **bot**。直接使用 API 的 bot 則透過 API key 跑在 daemon 內，不需 CLI 或終端 pane，專門擔任顧問（advisor）。你可以在瀏覽器或手機上看對話、回覆提問、即時掌握額度與 API 餘額，並讓多個 bot 互相協作。

AG Man 不是 IDE，也不自己直接啟動 CLI agent。終端 pane 由 herdr 管理；daemon（`agents-managerd`）負責和 herdr 溝通並透過各 CLI 的 hook 接收回覆，或直接與模型供應商 API 溝通。

> 截圖使用內建的假資料後端（`VITE_MOCK=1`），帳號、bot 與訊息都是示範資料。

![手機淺色主題：對話、額度晶片與 bot 切換](docs/screenshots/readme/en/mobile-light.png)

## 主要功能

**用對話取代盯終端機**
- 每個 CLI 工作階段或 API 顧問對應一個 bot，依專案目錄分組。可在瀏覽器送訊息、回合執行中排隊、中斷或強制停止。
- 回覆來自 CLI 的 hook、原生 transcript 或直接 API 串流。若 daemon 必須退回讀取終端快照，該則回覆會標示「可能不完整」。
- 確認與多選題會變成可點選的卡片。可以倒回對話（rewind）、fork bot 接續脈絡，也能喚醒閒置被收起來的 bot。
- 自動偵測閒置 bot 的終端草稿，顯示「編輯後送出」草稿列。
- 可隨時從設定面板或側欄「...」選單重啟 bot。

**協調多個 bot**
- **專家模式**（專案層級）：指定一位頂層管理者，其他成員為執行者（executor）、顧問（advisor）、排程者（scheduler）或專家（specialist，特定軌專家）。專家模式下不開子 agent，改由管理者派工給同專案成員。可透過 `manager_use` 個別開關成員以保護額度。
- **群集派工**：同專案內 kind、model、effort、identity 相同的執行者會合併成一個群集，可一次派工或整批開關。
- **API bot 擔任顧問**：`claude-api` 與 `openai-api` bot 跑在 daemon 內，透過 API key 運作，不需終端 pane。嚴格作為顧問（提供第二意見與背景諮詢），支援每回合成本上限（或設為無上限）與即時串流進度。
- **子 agent**：透過 herdr 啟動的子 agent 會掛在父 bot 底下；子 agent 完成或卡住時，父 bot 會收到通知。
- **總管（AGM）**：選用的協調者 bot，負責巡檢、派工、收起閒置 bot 與部署。見 [SPEC §18](docs/SPEC.md)。

**排程與推播**
- 專案排程：單次、固定間隔、每日、每週，以及多時段；排程 prompt 會送進專屬排程 bot（`role=scheduler`），支援快照／乾淨重啟（含 `agy`），並保留執行紀錄。
- 網頁推播（Web Push）：回合完成、需要回應、排程終態、上游新模型上線與額度重置時通知。可在設定中分別開關；若使用者已在任一裝置讀過該訊息、或正在手機前景使用 AG Man，就不再推播。
- 長訊息完成時推播簡明預覽「訊息較長，點擊查看完整內容」，點擊直接開啟對話。
- Codex 重置券提示可延後 1 小時、1 天或 3 天（「不再提醒」）。

**遠端主機與帳號額度**
- 專案可以放在 SSH 主機上，透過該主機的 herdr 運作：遠端 bot、主機 shell、CLI 登入、額度，也能把專案移交到另一台 daemon。
- **身份頁（Identities）**：瀏覽各主機上的身份，具備主機分頁與各 CLI 卡片（Claude、Codex、Grok、agy）。「新增帳號」對話框引導建立隔離設定目錄、提示共用風險、支援跨主機複製與同步身份，並於背景輔助完成登入（含自動擷取 `agy` 的 Google 授權網址）。預設帳號可隱藏或登出。
- **全域 API 餘額顯示**：頂部額度列與下拉選單即時顯示各主機的 Anthropic API 與 OpenAI API 餘額（OpenAI 直接讀取 platform `credit_grants` 實際值），桌機版具備雙行膠囊。
- **模型命名統一與排序**：Codex 與 OpenAI API 統一模型命名，模型依家族價格由便宜到貴排序（Luna < Terra < Sol < Astra；Flash < Hydrogen < Argon < Neon），舊版明確標註。
- 額度與重置時間依主機與身份顯示；低額度會警示。
- 模型、effort、fast mode 在 CLI 支援時可直接在網頁切換。

**更新與維運**
- 「更新檢查」頁列出每台主機的 herdr、各 agent CLI 與 hook 整合版本，可一鍵更新。
- 一次 CLI 升級收斂為單一可點擊晶片；安裝→重啟→排程串接由 daemon 在背景執行，關掉網頁也照跑。
- 閒置 bot 在設定的時間後會被收起，只保留恢復所需的資訊。

**工作區與防護**
- 可在對話旁預覽 bot 的本機 dev server，支援切換手機與 iPad 視口尺寸。
- outbox HTML 預覽與 `agy` 產圖：直接在新分頁完整預覽 bot 產出的 `.html` 檔案；`agy` 產生的圖片自動內嵌進聊天並同步列進 outbox（保留 1 小時）。
- 檔案架（跨 bot 暫存）與 outbox（bot 產出的可下載檔案，支援滑鼠與 iPad Apple Pencil 懸停預覽圖片）。
- 記憶體與 CPU 檢視：查看各主機整機 CPU% 與 herdr 行程樹。
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

網頁介面可在任何現代瀏覽器使用，桌機、iPad（直／橫向）與手機皆可。

## 需求與安裝

**必要**（Homebrew 與預先建置的執行檔方式會自動安裝 herdr；其餘需自行準備）

| 工具 | 用途 |
|---|---|
| [herdr](https://herdr.dev) | 終端工作區管理器。daemon 透過它的 Unix socket 開 pane、啟動 agent、送字、讀畫面。安裝後用 `herdr --version` 確認；設定頁的「更新檢查」會比對版本。 |
| 至少一個 agent CLI 或 API key | Agent CLI：`claude`、`codex`、`grok` 或 `agy`。API bot：Anthropic API key 或 OpenAI API key。 |
| Rust 工具鏈（`cargo`） | 只有從原始碼建置時需要。 |
| [Bun](https://bun.sh) | 只有從原始碼建置時需要（建置網頁前端）。 |

**建議安裝**（主機環境檢查會列出，缺少時會提示）

| 工具 | 用途 |
|---|---|
| `rtk` | token 壓縮代理；Claude 的 hook 會把一般指令改寫成它。 |
| `zsh`、`jq` | 主機 shell 與 hook 腳本使用。 |
| `gh` | 顯示 GitHub CLI 登入狀態。 |

**選用**：Blaze 建置引擎可加快開發建置，見 [`docs/BUILD-CACHE.md`](docs/BUILD-CACHE.md)。Claude API key、OpenAI API key 與 Jev 判斷層都是選用功能，預設關閉。

## 快速開始

**方式一：Homebrew（macOS 與 Linux，會一併安裝 herdr）**

```bash
brew install eden-sun/tap/agents-manager
brew services start agents-manager    # 停止：brew services stop agents-manager
```

服務會在登入後自動啟動並保持執行。若想在前景執行：`agents-managerd serve`（Ctrl-C 停止）。Log 位於 `$(brew --prefix)/var/log/agents-manager.log`。服務只會看到 Homebrew 的 `bin` 與 `~/.local/bin`；若你的 agent CLI 裝在其他位置，請改在 shell 中以前景方式執行。

**方式二：預先建置的執行檔**（網頁介面已內嵌）。到 [Releases](https://github.com/Eden-Sun/agents-manager/releases/latest) 下載對應平台的封存檔：`linux-x86_64`、`linux-arm64` 或 `macos-arm64`。

```bash
VERSION=v0.2.0          # 請用 Releases 頁最新的版本號
ASSET=linux-x86_64      # 或 linux-arm64 / macos-arm64
curl -fLO "https://github.com/Eden-Sun/agents-manager/releases/download/$VERSION/agents-managerd-$VERSION-$ASSET.tar.gz"
tar -xzf "agents-managerd-$VERSION-$ASSET.tar.gz"
install -m 755 "agents-managerd-$VERSION-$ASSET/agents-managerd" ~/.local/bin/
agents-managerd serve
```

每個封存檔旁都有 `.sha256` 檔可供核對。macOS 下載的執行檔可能需要先執行一次 `xattr -d com.apple.quarantine ~/.local/bin/agents-managerd`。封存檔不含 `scripts/` 下的腳本（例如 `agm`），需要的話請 clone 原始碼。

**方式三：從原始碼建置**（開發，或平台不在上面清單中）。以下以 clone 為例：

```bash
git clone https://github.com/Eden-Sun/agents-manager.git
cd agents-manager

# 1. 建置網頁前端（會被嵌入 daemon）
(cd web && bun install && bun run build)

# 2. 建置 daemon
cargo build --release -p agents-managerd

# 3. 啟動（前景執行，Ctrl-C 停止）
./target/release/agents-managerd serve
```

不論用哪一種方式，都在瀏覽器開啟 **http://127.0.0.1:7788**。從 `127.0.0.1` 開啟會自動登入；其他裝置請貼上 `~/.config/agents-manager/ui-token` 的內容。

想先看介面、不需要 herdr 或 daemon，可以用假資料後端：

```bash
cd web && bun install && VITE_MOCK=1 bun run dev
```

若要長期執行，Homebrew 的 `brew services` 會代為管理；其他方式可以用 systemd user unit、launchd，或放在一個 herdr pane 裡。Linux 主機的排程細節見 [SPEC §18.2e](docs/SPEC.md)。

## 第一次使用

1. **新增專案**：在側欄底部按「新增 Project」，選擇本機或 SSH 主機上的目錄，並取一個標籤。
2. **新增 bot**：打開專案的「＋」選單，選「新增 Bot」，選擇 kind（`claude`、`codex`、`grok`、`agy`、`claude-api` 或 `openai-api`），可選擇身份與模型。daemon 會開一個 herdr pane 啟動 CLI，或建立直連 API 顧問。
3. **對話**：在輸入框送出訊息。回合執行中送出的訊息會排隊；確認與多選題會以卡片顯示；停止按鈕可中斷回合。需要看原始畫面時切到「終端」分頁。
4. **讓 bot 互相委派**：bot 若執行 `herdr agent start …`，子 agent 會列在它底下，並在完成或卡住時通知它。專家模式下，管理者直接派工給專案內的執行者與顧問。
5. **自動化**：同一個「＋」選單有「新增排程」。帳號額度、模型清單、更新檢查與主機都在「環境設定」（Settings）裡。
6. **用手機開啟**：在同一個網路（LAN 或 Tailscale）上，用 `http://<本機位址>:7788` 開啟，並貼上 `ui-token`。

## 支援的 kind 與模型

AG Man 支援兩類 bot：
- **CLI bot**（`claude`、`codex`、`grok`、`agy`）：跑在 [herdr](https://herdr.dev) 終端 pane 內並注入 hook。可執行 shell 指令、編輯檔案並完整執行 agent 迴圈。
- **API bot**（`claude-api`、`openai-api`）：由 daemon 透過 API key 直接呼叫模型供應商 API，不需 CLI 或終端 pane。嚴格作為**顧問（Advisor）**運作（提供諮詢與第二意見），支援每回合成本上限（或無上限）。

> **注意**：daemon 會從各 CLI 與 API 供應商動態取得最新模型清單。下表為發布／撰寫時的快照（2026-11-27）。

| kind | 類型 | 說明 | 目前模型（依家族／等級） | effort 範圍 |
|---|---|---|---|---|
| `claude` | CLI | 跑在 herdr pane 的 Anthropic Claude Code | `opus`、`sonnet`（預設）、`haiku`、`fable` | `low`、`medium`、`high`、`xhigh`、`max` |
| `codex` | CLI | 跑在 herdr pane 的 OpenAI Codex CLI | `gpt-6-luna` < `gpt-6.1-terra` < `gpt-6.1-sol` < `gpt-6.2-sol`（預設） < `gpt-6-astra` < `gpt-6.2-astra` | `low`、`medium`、`high`、`xhigh`、`max`、`ultra`（luna 上限為 `max`） |
| `grok` | CLI | 跑在 herdr pane 的 xAI Grok CLI | `grok-4.7`（預設） | `low`、`medium`、`high`、`xhigh` |
| `agy` | CLI | 跑在 herdr pane 的 Google Antigravity | 依帳號：預設帳號 ag0、其他身份 ag1…；各帳號可用模型不同（例 ag0：Gemini 3.8 Flash＋Gemini 4 Neon／Argon／Hydrogen；ag1：只有 3.8 Flash），模型選單依主機＋帳號列出；context 視窗 1,048,576；有 1 小時 cache 倒數、不保溫 | 預設嵌在模型名稱內；閒置時可即時切換 `/model` 與 `/effort` |
| `claude-api` | API | 直連 Anthropic API（僅限 Advisor） | `claude-opus-5-5`（預設）、`claude-fable-5-2` | `low`、`medium`、`high`、`xhigh`、`max` |
| `openai-api` | API | 直連 OpenAI API（僅限 Advisor） | `gpt-6-luna` < `gpt-6.2-sol` < `gpt-6.2-astra`（預設） | `none`（luna）、`low`、`medium`、`high`、`xhigh`、`max` |

### 如何新增 Bot
- **網頁介面**：在專案的「＋」選單中選擇**新增 Bot**，選擇 kind、模型與 effort 等級（在專家模式下可選擇角色 role；API bot 只能為顧問 advisor）。
- **CLI（專家模式）**：
  ```bash
  agm hire <名稱> --kind <kind> --model <model> --effort <effort> [--role <role>] [--track <track>] [--identity <identity>]
  ```
  *（註：`agm hire` 只能聘執行者 `executor` [預設] 或專家 `specialist` [須附 `--track`]；顧問由使用者在網頁建立。執行者命名照規則 `<帳號>-<模型簡寫>-<序號>`。範例：`agm hire cc0-hk-1 --kind claude --model haiku --effort medium`、`agm hire ag0-ne-1 --kind agy --model gemini-4-neon-medium`，或指定身份 `agm hire cc1-hk-1 --kind claude --model haiku --effort medium --identity cc1`）*

### 支援的 agent CLI 與 hook 機制

每個 CLI 在啟動時會被注入 hook，因此回覆是結構化事件，而不是從畫面抓文字。

| kind | 回覆來源 | 備註 |
|---|---|---|
| `claude` | `--settings` 的 hook（`SessionStart`／`Stop`／`StopFailure`） | 多身份；模型與 effort 可即時切換 |
| `codex` | Stdin `Stop` hook 與 `notify` hook | 模型、effort、Fast 可即時切換 |
| `grok` | `GROK_HOME` 內的 hook 檔 | 模型可即時切換；effort 需重啟；見 [SPEC §12](docs/SPEC.md) |
| `agy` | Antigravity CLI 設定中的具名 hook | 標題列顯示更新提示；多身份（ag0、ag1…）；閒置時即時切換 `/model` 與 `/effort`；產圖自動收進聊天與 outbox；context 視窗 1,048,576；1 小時 cache 倒數、不保溫；見 [SPEC §12a](docs/SPEC.md) |
| `claude-api` | 直連 Anthropic API 串流（SSE） | 僅限 Advisor；每回合成本上限、prompt cache 倒數 |
| `openai-api` | 直連 OpenAI Responses API 串流（SSE） | 僅限 Advisor；每回合成本上限、reasoning tokens 顯示 |

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
- 設定頁（Settings）的分頁：Setup、Hosts、Update check、Identities、Jev suggestions、Claude API、OpenAI API、Display。

完整欄位見 [SPEC §5](docs/SPEC.md)。

## 安全注意

- **網路綁定**：打包好的 macOS app 只綁 `127.0.0.1`。其他情況下（包含直接執行 `agents-managerd serve`），daemon 預設會綁定所有介面，讓同一網路的手機也能連上。若只想給本機用，請設定 `AM_DEV_LAN=0`。
- **ui-token**：擁有它就等於擁有完整的網頁權限，包括啟動與停止 bot。請勿提交到 git、請勿貼到公開場合。共用 token 的 LAN 風險是有意保留的設計取捨，見 [API.md §0](docs/API.md)。
- **資料只在本機**：對話、設定與資料庫都存在你的機器上。遠端主機的專案則透過 SSH 存取該主機上的資料。
- **第三方服務預設關閉**：Claude API（一次性 LLM 工作、changelog 分診、訊息翻譯）、OpenAI API（顧問、翻譯）、Jev 判斷層（外部 API key）與分享入口都需要你明確開啟。翻譯按鈕是點擊才會扣費。簡體中文轉繁體則在本機完成，不送 API。
- **網頁推播**：推播內容會經過瀏覽器的推播服務（由各瀏覽器供應商提供）送達，內容加密，但推播服務會知道推送的時間與對象。可在設定中關閉。
- **分享 bot**：分享連結可讓受限或受信任的 bot 被其他人使用，屬於獨立的入口，見 [SPEC §20](docs/SPEC.md)。
- **廣域 kill 防護**：PreToolUse hook（claude、本機 grok）與 `pkill`／`killall` 的 PATH shim 會攔截可能殺掉其他 agent 的指令：無目標的 `pkill`／`killall`、符合 agent 行程名稱的 pattern、BSD `pkill` 會誤讀的選項順序、`kill -1`／`kill 0`。明確指定 PID 與 `lsof -ti :PORT | xargs kill` 則放行。請不要為了繞過它而改用別的寫法。各 CLI 的防護覆蓋率見 SPEC §4.4c。

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

## 授權

[MIT](LICENSE)
