# Antigravity CLI（agy）支援：調查與設計（第一階段）

> 狀態：MVP（§3.1，本機單一身分）已實作，現況寫在 `docs/SPEC.md` §12a 與附錄 G、`docs/API.md`；這份檔案**只剩第二階段（§3.2）與還沒驗的風險／取樣清單（§4、§6）仍有效**，第二階段做完就刪掉（AGENTS.md：文件只寫現況）。
> MVP 與本檔不同的決定（使用者 2026-10-04）：只用預設 `$HOME` 的 `~/.gemini`（不做 per-bot HOME、不做 `ag1`／`ag2` 身分切換與身分 UI），登入由使用者自己在 TUI 做，AG Man 不代按。
>
> 這份取代 Gemini CLI 那份設計（`dfdb89dd`，`docs/design/gemini-cli-support.md`）。Google 自 2026-06-18 起對個人用戶停了 Gemini CLI，
> 目標改為 agy。AG Man 端的整合點盤點（kind／DB／啟動參數／畫面判讀／送達／hook／額度／更新／web／測試隔離／文件）沿用該份，這裡只重寫「agy 怎麼接」。
>
> 調查對象：`agy` **1.2.16**（2026-10-04 install.sh 的 stable manifest；Go 單一執行檔，linux amd64／arm64／musl、darwin arm64／amd64、android）。
> 來源三種，標示如下：
> - **［實測］**：install.sh 只讀不執行；binary 下載到拋棄式目錄，用假 `HOME`、**未登入**（另有一次假 `GEMINI_API_KEY`，不是真 key）在 m4p（macOS arm64）的獨立 herdr session 裡跑 TUI。真實 HOME 沒有被碰（`~/.gemini` 不存在），測完 session、pane、暫存全部刪除。
> - **［官方］**：binary 啟動時自己展開到 `~/.gemini/antigravity-cli/builtin/skills/` 的內建文件（`hooks.md`、`plugins.md`、`rules.md`、`json_configs.md`）、`google-antigravity/antigravity-cli` 的 README／CHANGELOG／`examples/`、`antigravity.google/docs/cli/*`。
> - **［二手］**：教學站與第三方 issue（computingforgeeks、continuumcode、entireio/cli PR #1287、claude-remember #563、automatis-tools #177）。與官方或實測衝突時以前兩者為準，已逐項標明。
>
> **沒登入就驗不到的**集中列在 §6，MVP 開工前要先做一次「登入 spike」（§5 第 1 題）。

## 0. 結論先講

- **可以做，而且有三處比 grok／gemini 順**：
  1. **hook 有 `Stop`（回合結束）**，payload 帶 `conversationId`、`transcriptPath`、`terminationReason`、`error`、`fullyIdle`［實測，SessionStart／PreInvocation／Stop 三個都收到］；hook 子行程繼承整份 pane env（`AM_*` 傳得進去）。
  2. **`statusLine`／`title` 指令是現成的結構化狀態管道**：每次 agent 狀態改變，TUI 就把一份 JSON 餵給指令［實測］，裡面有 `agent_state`、`tool_confirmation_pending`、`pending_input_count`、`conversation_id`、`transcript_path`、model、context 用量，登入後還有 `quota`（各模型 bucket 的剩餘比例與重置時間）、`plan_tier`、`email`［官方 schema］。
     title 指令的輸出會成為 OSC 視窗標題，herdr 的 `terminal_title` 讀得到［實測：標題變成我們腳本印的字］。這等於免費拿到「畫面判讀」缺的那一半，也是 herdr 判 idle 不可信時的主訊號。
  3. **信任框可預寫**：接受後寫進 `~/.gemini/antigravity-cli/settings.json` 的 `trustedWorkspaces`（絕對路徑陣列）［實測］，不必代按鍵。
- **最大的坑（照風險排序見 §4）**：
  - **agm-host 這顆 CPU 跑不起來 agy**：glibc 與 musl 兩個 build 都在啟動時 `FATAL ERROR: This binary was compiled with pclmul enabled, but this feature is not available on this processor`［實測；agm-host 是 QEMU Virtual CPU 2.5+，沒有 pclmulqdq］。要嘛 VM 改 `cpu=host`，要嘛先只在 m4p 做。
  - **設定目錄只認 `$HOME`**［實測：`XDG_*`、`ANTIGRAVITY_EXECUTABLE_DATA_DIR` 都無效，檔案全落在 `$HOME/.gemini/`］。身分隔離只能換整個 HOME，會連帶換掉 git／ssh／gh 設定；憑證放 OS keyring（macOS 走 Keychain，service 名未知，推測不隨 HOME 隔離；Linux 無 D-Bus 才落檔），多帳號能不能成立要登入後才知道。
  - 登入框、首次啟動的色彩／條款頁、信任框，herdr 全判 `idle`（含 `interactive_ready:true`）［實測］，跟 gemini 同一種坑；而且 herdr 內建的 agy 偵測規則是 2026-06-24 的舊版，1.2.16 的權限框文案已改（見 §1 A.8），**阻塞狀態可能整個漏掉**。
  - agy **預設背景自我更新**；官方 install.sh 還會跑 `agy install` 改 shell profile（含「alias purging」）。
  - 首次條款頁**預設已勾選**「允許 Google 收集並使用我的 Interactions 資料」［實測］——跑私有 repo 的 bot 要先決定這件事。
- `bots.kind` 有 SQLite `CHECK (kind IN ('claude','codex','grok'))`，加 kind 要重建表＋升 `SCHEMA_VERSION`（同 gemini 設計 §2 #1）。kind 名建議就叫 **`agy`**：herdr 的 `agent start --kind agy` 已存在（herdr 0.9.3，manifest `agy` 版本 2026.06.24.1，別名 `antigravity`／`antigravity-cli`）［實測：`herdr agent start … --kind agy` 在 PATH 上有名為 `agy` 的執行檔時成功，`agent: agy`］。

---

## 1. agy 事實（1.2.16）

### A.1 安裝與執行檔
- `curl -fsSL https://antigravity.google/cli/install.sh | bash`。腳本做的事［實測，只讀］：查
  `https://antigravity-cli-auto-updater-974169037036.us-central1.run.app/manifests/<platform>.json`
  （`linux_amd64`、`linux_amd64_musl`、`linux_arm64`、`darwin_arm64`…），取 `{version, url, sha512}`；下載 `.tar.gz` 到 `~/.cache/antigravity/staging/`，**sha512 比對**後解出 `antigravity`，複製成 `~/.local/bin/agy`（`-d/--dir` 可換目錄），最後執行 **`agy install`**（改 shell PATH 與 alias；`--skip-path`／`--skip-aliases` 可略過，後者的說明是 "Bypasses shell profile alias purging"）。已存在 `agy` 時直接退出並說「agy 會在背景自己更新」。
- 1.2.16 的 linux-x64 tarball 解開是 **200 MB** 的靜態 Go 執行檔（只含 `antigravity` 一個檔）。**不需要 node**。
- 行程名 `agy`（單一行程；內建一個 language server，log 裡有 localhost 的隨機 gRPC／HTTP 埠，不影響 AG Man）。
- 子命令［實測 `--help`］：`agent(s)`、`changelog`、`help`、`install`、`mcp`、`mic-serve`、`models`、`plugin(s)`、`remote-control`、`update`。**沒有** `login`／`logout`／`status`。
  `models`、`agents` 支援 `--output-format json|stream-json`［CHANGELOG］；`models` 未登入時報 `Please sign in to view available models`［實測］。

### A.2 旗標（`agy --help`，實測；括號為官方補充）
| 旗標 | 意義 | AG Man 用途 |
|---|---|---|
| `--model <slug>` | 模型。slug 含 effort 變體：`gemini-3.1-pro-high`／`-low`、`gemini-3.8-flash-medium`…［二手＋實測：帶 `gemini-3.1-pro-high` 後橫幅顯示 `Gemini 3.1 Pro (High)`］。無法解析時互動模式退回預設並警告，`-p` 直接失敗［CHANGELOG］ | `bots.model` |
| `--effort low\|medium\|high\|xhigh\|max` | 另外指定 reasoning effort（各模型支援的檔位不同） | 第二階段 |
| `--dangerously-skip-permissions` | 自動核准所有工具權限 | `auto_approve` |
| `--mode accept-edits\|plan` | 執行模式；TUI 內 shift+tab 循環 `default → accept-edits → plan`［實測：`--mode plan` 輸入框顯示 `Plan mode: research & plan only (shift+tab to cycle)`］ | 暫不用 |
| `-c, --continue` | 接最近一段對話［實測：同 cwd 下接回上一段］ | resume（備援） |
| `--conversation <id>` | 接指定對話；id 不存在則**警告並忽略**（開新的）［CHANGELOG］。離開時 TUI 印 `Resume with -c (or command below):` ＋ `agy --conversation=<uuid>`［實測］ | resume |
| `-i, --prompt-interactive "<p>"` | 先跑這句再進互動 | 暫不用 |
| `-p, --print`／`--output-format text\|json\|stream-json`／`--input-format stream-json`／`--json-schema`／`--print-timeout` | headless。`stream-json` 輸入可「一個行程多回合」［CHANGELOG］ | 非目標（§3.3）；但是個日後的選項 |
| `--add-dir`／`--agent`／`--project`／`--new-project`／`--sandbox`／`--remote-control`／`--log-file` | 工作區、agent、專案、終端沙盒、遠端控制、log 路徑 | 暫不用 |

**沒有**：fork 旗標（TUI 內有 `/fork`、`/rewind`）、「啟動時指定新對話 id」的旗標、`--append-system-prompt`、`--config-dir`。

### A.3 設定目錄與身分隔離［實測］
- 全部在 **`$HOME/.gemini/`** 底下，且**只認 `$HOME`**：
  - `~/.gemini/antigravity-cli/`：`settings.json`（使用者設定；TUI 設定會回寫並**重排版**）、`brain/<conversation-id>/`、`conversations/<id>.db`（SQLite）、`conversation_summaries.db`、`cache/onboarding.json`、`installation_id`、`jetski_state.pbtxt`、`log/cli-<時間>.log`（`cli.log` 是 symlink）、`updater/{update.lock,update_status.json}`、`last_check.timestamp`、`builtin/`（內建 skills／文件，每次啟動校驗 `.checksum`）。
  - `~/.gemini/config/`：全域 customization 根目錄——`hooks.json`、`mcp_config.json`、`plugins/`、`rules/`、`AGENTS.md`／`GEMINI.md`（全域規則）、`config.json`（plugin 開關）、`projects/`。`~/.gemini/GEMINI.md`、`~/.gemini/AGENTS.md` 也讀（與 Gemini CLI 共用同一個 `~/.gemini`！）。
- **沒有任何環境變數能改位置**：試過 `XDG_CONFIG_HOME`／`XDG_DATA_HOME`／`XDG_STATE_HOME`／`ANTIGRAVITY_EXECUTABLE_DATA_DIR`，檔案仍落在 `$HOME/.gemini`。
  → 身分（identity）隔離 = **換 `HOME`**。比 `CLAUDE_CONFIG_DIR`／`CODEX_HOME`／`GROK_HOME` 粗：bot 子行程看到的 `~` 都變了（git config、ssh key、gh、npm、cargo…）。
  可行做法：每個身分一個資料夾 `~/.agy-ag1/`，內含 `.gemini/`，其餘（`.ssh`、`.gitconfig`、`.config/gh`…）用 symlink 指回真 HOME，啟動時以 `env HOME=~/.agy-ag1 agy …` 只包 agy 本身。
  但 agy 開出的工具子行程（bot 讓 agy 跑的 git、ssh、npm…）會繼承那個假 HOME，所以 symlink 要補齊——取捨見 §5 第 3 題。
- 工作區規則：`GEMINI.md`／`AGENTS.md` 從 cwd 往上走到 repo 根自動載入［官方 rules.md］——**本 repo 的 `AGENTS.md` 會被 agy 自動讀到**；`.agents/`（rules／skills／hooks.json／mcp_config.json／agents）是專案層 customization 根。

### A.4 環境變數
- 官方文件列出的：`AGY_CLI_DISABLE_AUTO_UPDATE=true`（關自動更新）、`GEMINI_API_KEY`（搭配 settings `"modelProvider":"gemini"`）、`GOOGLE_GEMINI_BASE_URL`、`EDITOR`。
- binary 字串裡另有（未文件化，僅供參考）：`AGY_CLI_HIDE_LOGO`、`AGY_CLI_HIDE_ACCOUNT_INFO`、`AGY_CLI_DISABLE_LATEX`、`AGY_CLI_FORCE_OSC`、`AGY_CLI_INTERACTIVE_HEADLESS`、`AGY_CLI_NONINTERACTIVE_HEADLESS`、`AGY_CLI_MODEL_API_MAX_RETRIES`、`AGY_LLM_GATEWAY_{URL,API_KEY,MODELS,PROXY_URL,WIRE_PROTOCOL,HEADERS,CA_CERT}`、`AGY_PLUGIN_AUTH_CALLBACK_PORT`、`AGY_REMOTE_CONTROL_VERSION_OVERRIDE`、`ANTIGRAVITY_*`（內部用）。不要依賴。
- agy 會對 hook 子行程設 **`ANTIGRAVITY_CONVERSATION_ID`**［實測］。

### A.5 settings.json（`~/.gemini/antigravity-cli/settings.json`）
- 官方 reference 列的鍵：`colorScheme`、`altScreenMode`、`toolPermission`（`request-review`／`proceed-in-sandbox`／`always-proceed`／`strict`）、`artifactReviewPolicy`、`notifications`、`editor`、`editorMode`、`allowNonWorkspaceAccess`、`enableTerminalSandbox`、**`useG1Credits`**（額度用完時動用個人 AI Credits，預設 false）、`enableTelemetry`（預設 true）、`verbosity`…；另有 `modelProvider`、`permissions.allow[]`（例 `command(git)`、`write_file(src/)`、`mcp(github/*)`）、`statusLine`、`title`、`trustedWorkspaces`、`pickerGrouping`。
- **`statusLine`／`title`**：`{"type":"command","command":"<路徑>"}`［實測可用］；可選 `padding`、`enabled`、`stack_with_default`。TUI 在 agent 狀態變化時執行該指令，stdin 給 JSON，stdout 當顯示字串（title 會去掉控制字元）。
- **`trustedWorkspaces`**：接受信任框後 agy 自己寫入［實測：`"trustedWorkspaces": ["<cwd 絕對路徑>"]`］。
- 解析失敗時 agy **拒絕覆寫**並在狀態列點名那個檔［CHANGELOG 1.2.x］。AG Man 寫這個檔必須 merge（保留未知鍵）、原子寫入，且要預期 agy 隨時重排版回寫。
- `modelProvider:"gemini"` ＋ `GEMINI_API_KEY`：不用登入即可啟動 TUI，橫幅第二行顯示 `Gemini API key`［實測，用假 key：送一句話得 `API key not valid`，Stop hook 的 `terminationReason` 是 `ERROR`，`error` 帶完整錯誤文字］。

### A.6 Hooks［官方 hooks.md ＋ 實測］
- 設定檔：**`~/.gemini/config/hooks.json`**（全域）與 `<workspace>/.agents/hooks.json`（專案層，需信任資料夾）；plugin 內的 `hooks.json` 也會併入。格式：頂層 key = hook 名稱，其下為事件：
  ```json
  { "agents-manager": {
      "enabled": true,
      "SessionStart":   [ { "type": "command", "command": "…", "timeout": 5 } ],
      "PreInvocation":  [ { "type": "command", "command": "…", "timeout": 5 } ],
      "PostInvocation": [ { "type": "command", "command": "…", "timeout": 5 } ],
      "Stop":           [ { "type": "command", "command": "…", "timeout": 5 } ],
      "PreToolUse":     [ { "matcher": "*", "hooks": [ { "type": "command", "command": "…" } ] } ],
      "PostToolUse":    [ { "matcher": "*", "hooks": [ { "type": "command", "command": "…" } ] } ] } }
  ```
  **形狀有兩種**：`PreInvocation`／`PostInvocation`／`Stop` 是**扁平**的 handler 陣列；`PreToolUse`／`PostToolUse` 要包 `{matcher, hooks:[…]}`。形狀錯了整個檔會被丟掉或警告（多個第三方工具的 issue 都因此壞掉）。同事件的多個具名 hook 合併、依序執行。
- 官方文件只列五個事件（`PreToolUse`、`PostToolUse`、`PreInvocation`、`PostInvocation`、`Stop`）；**`SessionStart` 沒有文件，但 1.2.16 實際會載入並觸發**［實測，與第三方 issue 一致；`SessionEnd`／`Notification`／`UserPromptSubmit`／Gemini 事件名都不認］。→ AG Man 不可只靠 `SessionStart`，要能用第一個 `PreInvocation` 補身分。
- 時機［實測］：**對話是第一則 prompt 才建立**——啟動後、送第一句前，沒有 conversation id（statusLine 的 `conversation_id` 為空、`transcript_path` 是占位路徑）；送出第一句時依序觸發 `SessionStart` → `PreInvocation`（`invocationNum:0`、`initialNumSteps:1`）→（出錯或完成）`Stop`。
- 共通輸入（stdin，camelCase protojson）［實測］：`conversationId`、`modelName`（例 `gemini-3.1-pro-low`）、`transcriptPath`（`…/brain/<id>/.system_generated/logs/transcript_full.jsonl`）、`artifactDirectoryPath`、`workspacePaths[]`。**payload 沒有事件名**——事件名要靠命令列參數帶。`Stop` 另有 `executionNum`、`terminationReason`（實測 `ERROR`；官方文件寫 `model_stop`／`max_steps_exceeded`／`error`；第三方觀察到 `NO_TOOL_CALL`——**值域不可假設，只當字串記錄**）、`error`、`fullyIdle`（有背景工作時為 false）。**沒有 prompt 文字、沒有助理回覆文字**，回覆要從 `transcriptPath` 讀。
- 輸出（stdout JSON）：`PreInvocation` 可回 `{"injectSteps":[{"ephemeralMessage"|"userMessage"|"toolCall":…}]}`；`PostInvocation` 可回 `terminationBehavior: force_continue|terminate`；`Stop` 回 `{"decision":"continue","reason":"…"}` 會**擋住停止、把 reason 當訊息塞回去**（有連續次數上限）；`PreToolUse` 回 `allow|deny|ask|force_ask`。AG Man 的 hook 一律印 `{}` 且 exit 0（實測 `{}` 被接受）——**絕不能回 `decision: continue`**。
- 其他行為：hook 是**同步**的、會卡住 agent loop，逾時預設 30 秒；工作目錄是 `hooks.json` 所在資料夾（全域時是 `~/.gemini/config`，**不是** bot 的 cwd，要用 payload 的 `workspacePaths[0]`）；hook 子行程**繼承整份 pane env**［實測：`AM_*` 與 `GEMINI_API_KEY` 都在］；hook 載入不需信任專案（全域檔在啟動時就載入，log：`loaded 1 named hooks from 1 hooks.json file(s)`）。

### A.7 Session（對話紀錄）
- 每段對話：`brain/<id>/`（artifact、`.system_generated/logs/{transcript.jsonl,transcript_full.jsonl,chunks/…}`）＋ `conversations/<id>.db`（SQLite＋WAL）＋ `conversation_summaries.db`（索引，供 `/resume` 選單）。**不要讀 `.db`**。
- `transcript_full.jsonl`［實測，只有 user 那一行］：`{"step_index":0,"source":"USER_EXPLICIT","type":"USER_INPUT","status":"DONE","created_at":"…Z","content":"<USER_REQUEST>\n…\n</USER_REQUEST>\n<ADDITIONAL_METADATA>…</ADDITIONAL_METADATA>…"}`。使用者原文被包在 `<USER_REQUEST>` 標籤裡，後面附 metadata 與 settings 變更說明——讀者要剝掉。模型回覆與工具步驟的 `type` 值（binary 內有 `PLANNER_RESPONSE`、`RUN_COMMAND`、`NOTIFY_USER` 等 `CORTEX_STEP_TYPE_*`）與欄位（第三方描述有 `tool_calls`）**未實測**，見 §6。
- 保留期限與 `--conversation` 跨 cwd 行為未查到；`-c` 在同一 cwd 接得回［實測］。
- 同一對話在另一個 CLI 實例已開啟時，會顯示建議用 `/fork` 的非阻塞橫幅［CHANGELOG］。

### A.8 TUI 畫面［實測，未登入＋API key 模式］
- 首次啟動序列（每一頁 herdr 都判 `idle`）：①（未登入）`Welcome to the Antigravity CLI. You are currently not signed in.` ＋ `Select login method:` `> 1. Google OAuth` / `2. Use a Google Cloud project`；② `Choose your color scheme:`（含預覽）；③ `Terms of Service & Data Use`——**`> [x] Yes, I agree to help improve Antigravity CLI by allowing Google to collect and use my Interactions data…` 預設勾選**，Enter 切換，往下選 `Done` 才確認；④ `Accessing workspace:` ＋ `Do you trust the contents of this project?` ＋ `> Yes, I trust this folder` / `No, exit`；選 No 會直接退出。完成後 `cache/onboarding.json` 寫 `consumerOnboardingComplete:true`。
- 主畫面：頂部橫幅四行（logo ＋ `Antigravity CLI 1.2.16` / **憑證種類**（`Gemini API key` 或帳號）/ **目前模型**（`Gemini 3.1 Pro (Low)`）/ cwd），一條分隔線，輸入框 `>`，分隔線；錯誤以 `⚠ agent executor error: …` ＋ `Error ID:` 顯示。
- 斜線指令［實測：`/model` 開「Switch Model」選單（模型列＋effort 滑桿），`/usage` 開「Models & Quota」面板（未登入卡在 `Loading quota summary…`），`/quit` 退出］；官方另有 `/clear`、`/resume`、`/fork`、`/rewind`、`/permissions`、`/credits`、`/hooks`、`/config`、`/statusline`、`/title`、`/logout`。`-p "/usage"` 之類唯讀指令可**不開 agent 回合**直接印結果（tab 分隔，或 `--output-format json`）［CHANGELOG］。
- 退出：`/quit`、`ctrl+d` 兩次；`ctrl+c` 中斷 agent、閒置時退出［二手］；`esc` 中斷生成（`Press esc to interrupt generation.`）。agy 自己有**訊息佇列**（`pending_input_count`；設定 `Send Immediately`）——跟 AG Man 的 queue 會疊在一起。
- 權限確認文案（1.2.x）：`Run this command?`／`Allow access to this URL?`／`Allow calling this tool?`＋`Reason:` 行［CHANGELOG 1.2.x］。**herdr 的 agy manifest（2026.06.24.1）還在找舊字串 `requesting permission for:` ＋ `do you want to proceed?`／`tab amend`**，working 規則是「braille spinner ＋ 以 `ing` 結尾的字」、背景工作規則是 `· N task`。→ 1.2.16 的權限框是否被判 blocked **未驗證**，應假設不會。
- 預設 OSC 標題未測（我們的 title 指令接管了）。

### A.9 登入
- README：先試 **OS keyring**（Keychain／Secret Service／Credential Manager），沒有就 Google Sign-In；本機自動開瀏覽器；**偵測到 SSH** 時印授權網址，在別處完成後貼回。Linux 沒有 D-Bus session bus 時**自動略過 keyring**（headless 主機、容器）［CHANGELOG］，改走檔案儲存（**檔案位置未查到，需登入後看**）。binary 內有 `keyring_detector_{ssh,dbus,container,termux,wsl}`——SSH／容器環境會被判成「無 keyring」。
- 登入只能在 TUI 內（沒有 `agy login`）；`/logout` 清除。`-p` 模式下，授權碼可從 `/dev/tty` 貼入［CHANGELOG］。
- 三種憑證：①Google OAuth（個人帳號，免費層有每週限額［二手］／Google AI 訂閱）；②GCP 專案／ADC（企業；`AGY_ADC_AUTH`）；③`GEMINI_API_KEY`（`modelProvider:"gemini"`；按量計費；模型只剩 Gemini 系列——選單只列 Gemini 3.8／3.7／3.6 Flash 與 3.1 Pro［實測］）。
- 已登入判讀：沒有 status 指令。登入後 statusLine payload 的 **`email`**、**`plan_tier`** 可用［官方 schema］；橫幅憑證行亦可。

### A.10 額度
- `/usage`、`/credits`、`/quota`（TUI）與 `-p "/usage"`（不耗額度）；statusLine payload 的 **`quota`**（各模型 bucket：`remaining_fraction`、`reset_time`、`reset_in_seconds`）［官方 schema；API key 模式的 payload 沒有 `quota` 鍵，實測］。個人免費層是**每週**限額［二手］。
- 撞限行為［CHANGELOG］：伺服器給 retry delay 時照等；delay > 30 秒或是每日／帳單上限就**立刻停**；方案額度用完且 AI Credits 不夠時顯示 `Your AI credits balance is too low to continue.`。`useG1Credits`（用個人 credits 續跑）預設 false——**AG Man 不得開**。撞限的阻塞對話框（若有）**未見到**。
- Stop hook 的 `error` 欄帶完整錯誤文字（實測有 HTTP 400 與 `API_KEY_INVALID`），可餵 `hookrecv::classify_failure` 認 429／quota。

### A.11 自動更新
- 預設**背景自我更新**（install.sh：「agy 會在一般執行時於背景自我更新」）；`~/.gemini/antigravity-cli/updater/update.lock`＋`last_check.timestamp`（15 分鐘 TTL）；`agy update`（手動）；`AGY_CLI_DISABLE_AUTO_UPDATE=true` 關閉［官方；binary 內有此字串；**沒有在有新版的情況下實測關閉有效**，見 §6］。
- 上游版本來源：manifest JSON（A.1），含 sha512——**比 npm 好驗**。
- 版本節奏很快：二手文章測 1.2.4，今天 1.2.16；CHANGELOG 886 行。

---

## 2. 整合點對照表（grok／gemini 設計 → agy）

沿用 gemini 設計的編號。「同」＝照 gemini 設計，不重寫。

| # | 整合點 | agy 設計 | 風險／待驗 |
|---|---|---|---|
| 1 | kind 列舉：`config.rs KINDS`、`db.rs` CHECK 重建＋升 `SCHEMA_VERSION`、`mission/api.rs`、`reconcile.rs`、`default_session.rs`、`update_watch.rs`、web `BotKind` | 同，kind＝`agy` | 與別人同時升 schema 要合流 |
| 2 | 權限參數：`setup.rs permission_args` | `auto_approve` → `--dangerously-skip-permissions`；否則不帶。`toolPermission` 留預設 `request-review` | 使用者 settings 若寫 `always-proceed` 會繞過 AG Man 的 auto_approve 開關（只讀、不改） |
| 3 | model／effort：`model_args`、`efforts_for_kind`、`models.rs`、`ModelPicker` | `--model <slug>`；**effort 已包含在 slug**（`-high`／`-low`…），MVP `efforts_for_kind("agy")=[]`；清單用 `agy models --output-format json`（在 bot 的 HOME 下跑、需登入）。第二階段才加 `--effort` | 清單隨帳號／憑證不同（API key 只有 Gemini）；slug 命名會變；`--model` 解析失敗互動模式只警告→要從橫幅或 statusLine `model.id` 回讀驗證 |
| 4 | 啟動前置與 hook 注入：`setup.rs`、`lifecycle/*_hook.rs` | 寫（merge）`<HOME>/.gemini/config/hooks.json` 的 **`agents-manager`** 具名 hook：`SessionStart`／`PreInvocation`／`Stop`（`timeout:5`）→ 指令 `agents-managerd hook agy --event <名>`（遠端用 `hook.sh agy <bot> <event>`）；分派靠 env（`AM_BOT_ID` 空就 no-op，仿 grok 的 env 分派腳本，這樣預設身分共用真 HOME 時別的 agy 也不誤報）。hook 一律印 `{}`、exit 0 | ① 形狀（扁平 vs matcher 包）寫錯整檔失效→要有單元測試釘形狀；② 與別的工具共用同一個 `hooks.json`→merge 只動 `agents-manager` 鍵並原子寫；③ hook 同步卡 loop→hook 程式必須亞秒級、失敗也吞掉；④ `SessionStart` 無文件 |
| 5 | 信任：`trust.rs pretrust_for_start`、`tui_prompts` | **預寫 `settings.json` 的 `trustedWorkspaces`**（merge、原子寫），不代按鍵；另寫 `is_agy_trust_dialog`（`Do you trust the contents of this project?`）只做偵測→blocked 原因 | 未驗證 `--dangerously-skip-permissions` 是否也跳過信任框；agy 隨時重排版回寫該檔→寫入要在啟動前、且重讀再 merge |
| 6 | 回報管道：`hookrecv::classify`、§4.1 provider＝kind 守衛 | 第一個 `PreInvocation`（或 `SessionStart`）→Identity（`conversationId`、`transcriptPath`、`workspacePaths[0]`）；`PreInvocation`→送達證據；`Stop` 且 `fullyIdle==true` 且非 `continue`→TurnComplete（`assistant` 從 transcript 取）；`Stop.terminationReason=="ERROR"`→失敗（`error` 文字進 `classify_failure`）；`fullyIdle==false`→回合尚未真正結束，不出 TurnComplete | ① 事件名不在 payload→靠參數；② `Stop` 沒有助理文字→TurnComplete 要等 transcript 可讀；③ 子 agent 繼承 pane env 會用父 bot 的 hook：shim 的 `AM_RESERVED_ENV_KEYS` 已含 `AM_*`，但 `HOME` 不在清單且不該在——改以 payload `conversationId` 必須等於 run 的 native id（首個 PreInvocation 後鎖定）守衛 |
| 6b | **statusLine／title 信標（agy 獨有）** | 同一份 `settings.json` 設 `statusLine`＝無害的小指令（stdin JSON → 轉給 `agents-managerd hook agy --event state`，stdout 印空字串或簡短狀態），`title`＝輸出 `agy\|<agent_state>\|confirm=<0/1>` 這類固定格式；daemon 用 payload 的 `agent_state`／`tool_confirmation_pending`／`pending_input_count` 判 idle／working／blocked，**不依賴 herdr 的 agent_status 與畫面判讀**；登入後的 `quota`、`email`、`plan_tier`、`context_window` 同步入庫 | 要動使用者的 `settings.json`（預設身分共用真 HOME 時＝改使用者的檔，見 §5 第 3 題）；statusLine 指令也是同步執行，要快；`agent_state` 值域（`authenticating`／`initializing`／`idle`／`working`／`thinking`／`tool_use`…）會長新值→未知值當 working；自訂 statusLine 會取代使用者原有的（可用 `stack_with_default`） |
| 7 | native session id：`runs.native_session_id`、`resume_gate` | **無法預先指定**（不像 gemini 的 `--session-id`）。新對話的 id 在送第一句時才出現：由首個 hook／statusLine 回填。→ 比照 grok：id 為空時 `resume_gate` 不放行 resume；啟動後若一直沒送過 prompt 就沒有可 resume 的對話 | 對「剛啟動就重啟」的 bot 沒有 id 可接→退回 `-c`（同 cwd 最近一段） |
| 8 | resume：`start.rs resume_args_by_kind`、`bulk_restart`、`idle_sleep` | `--conversation=<uuid>`（有 id 時）否則 `-c`；同 cwd | 換身分＝換 HOME：要複製 `brain/<id>/`、`conversations/<id>.db(+wal/shm)`、`conversation_summaries.db` 的對應列→**風險高，第二階段再議**；不能複製就換身分時開新對話 |
| 9 | fork：`start.rs fork_args_by_kind`、`fork.rs` | 沒有旗標。TUI 有 `/fork`——第二階段才評估「送 `/fork` 再從 statusLine 讀新 `conversation_id`」 | 未實測 `/fork` |
| 10 | 身分隔離：`pane_identity::config_dir_var`、`tools.rs`、`IdentitiesPanel` | **換 `HOME`**（A.3）。身分名建議 `ag0`／`ag1`…，資料夾 `~/.agy-ag1/`（含 `.gemini/`，其餘 symlink 回真 HOME）；預設身分＝真 `~` | Mac 的 Keychain 不隨 HOME 隔離→同機多帳號可能互相覆蓋（待驗 §6）；`~/.gemini` 與 Gemini CLI 共用 |
| 11 | 登入／探測：`login_status_args`、`login_assist` | 探測：**不跑 CLI**——有 hook／statusLine 後讀最近一次 payload 的 `email`／`plan_tier`；沒資料時橫幅憑證行；登入：host-shell pane 跑 `agy`（SSH 會印網址＋碼；本機會開瀏覽器→**遵守規則**：在沒有瀏覽器的環境走 SSH 偵測／`NO_BROWSER` 類路徑並把網址交使用者），選 `Google OAuth`、讀網址、送回碼、`/quit` | 沒有 `agy login`；API key／ADC 身分沒有「帳號名」 |
| 12 | 畫面判讀：`screen.rs`、`delivery.rs`、`poller.rs`、`composer_draft.rs`、`tui_prompts.rs`、`events.rs PLACEHOLDER_TITLES` | 主訊號改用 #6b；畫面只做**防呆**：登入框／色彩頁／條款頁／信任框／（猜測的）權限框＝blocked，**不代按鍵**；空輸入框＝`>` 行夾在兩條分隔線之間；`Press esc to interrupt generation.`＝working | 回音字元與回覆標記要登入後實測；權限框 1.2.16 文案要實測；herdr agy manifest 太舊→可考慮用 `herdr server reload-agent-manifests` 載入本地覆寫（另案） |
| 13 | prompt 送達：`delivery.rs` | 以 `PreInvocation`（`invocationNum==0`）當送達證據（無 prompt 文字→與 queue 的預期文字比對改用 transcript 的 `USER_INPUT`，需剝 `<USER_REQUEST>`）；畫面回音當備援 | agy 自己有訊息佇列：忙碌中送字可能被收進它的佇列而非立即生效→`busy_send` 邏輯要核對 |
| 14 | transcript 讀取 | 新 `agy_transcript.rs`：讀 `transcriptPath`（`transcript_full.jsonl`，JSONL）；剝 `<USER_REQUEST>`；模型回覆 `type`（推測 `PLANNER_RESPONSE`）與工具步驟欄位**待登入後取樣** | 格式未文件化、版本節奏快→讀者對未知 `type` 寬鬆略過 |
| 15 | persona／AG Man 指示：`persona_args` | 首選：**per-bot HOME 下的全域規則檔** `<HOME>/.gemini/config/AGENTS.md`（寫「讀 X 檔並照做」；單檔上限 24 KB、規則總預算 20k tokens）；repo 的 `AGENTS.md` 本來就會被讀。備案：首個 `PreInvocation`（`invocationNum==0`）回 `injectSteps:[{"userMessage":…}]`。persona 必須寫明 AG Man 的「禁用內建子代理」規則對應 agy 的 `invoke_subagent`／`--agent`／`/btw` | 預設身分共用真 HOME 時不能寫全域規則（會污染使用者與 Gemini CLI）→改備案；`injectSteps` 的 userMessage 是否出現在對話歷史未驗證 |
| 16 | 子 agent（herdr shim、`child_identity`） | 同 grok：子 agent 無 hook，靠 transcript；身分從 `ps eww` 讀 `HOME` | herdr 0.9.3 `integration install` 沒有 agy |
| 17 | 額度：`quota.rs`、新 `quota_agy.rs`、`QuotaStrip`、`turn_error.rs` | **不用探測 session**：statusLine payload 的 `quota` 就是資料源（各模型 bucket）；個人免費層是每週窗。現有 5h／7d／F 三格要加「依模型 bucket」的呈現；撞限靠 `Stop.error` 文字＋`Your AI credits balance is too low` | API key 模式沒有 `quota` 鍵；撞限對話框未見；絕不開 `useG1Credits` |
| 18 | 版本與更新：`upstream_update.rs`、`cli_update.rs`、`update_watch.rs`、`host_baseline` | 上游＝manifest JSON（A.1，含 sha512）；安裝＝照 install.sh 的步驟**手動**做（下載→驗 sha512→複製為 `~/.local/bin/agy`），**不要跑 `agy install`**（改 shell profile／purge alias）；每個 agy 啟動都帶 `AGY_CLI_DISABLE_AUTO_UPDATE=true`（包含使用者手動在 bot pane 跑的——寫進 pane env） | 關閉旗標未實測；二進位 200 MB；`update.lock` 殘留會讓更新卡住 |
| 19 | 快取倒數：`cache_clock.rs` | `None`（headless JSON 有 `cache_read_tokens`，TUI 路徑沒有） | — |
| 20 | 其他 kind 分支：`turn_error.rs`、`slash.rs`、`composer_draft.rs`、`memproc.rs`、`events.rs`、`judge.rs`、`release_triage` | `/model` 開的是選單（不是直接套用）→AG Man 改模型走「重啟帶 `--model`」；清草稿鍵未知（`esc` 是中斷生成，不確定清不清草稿）；`release_triage::KINDS` 不加 | 待實測 |
| 21 | 測試隔離：`test_home.rs`、`home.rs` | 一律走 `crate::home::dir()`；agy 檔案全在 `$HOME/.gemini` 下，測試 HOME 是假的就天然隔離；測試行程不需額外清 env | — |
| 22 | web：`types.ts`、`kindMeta.ts`、`KindTag.tsx`、`kindTag.css`（`--kind-agy`）、`IdentitiesPanel`、`ModelPicker`、`QuotaStrip`、`MergedUpdateChip`、`mock.ts` | 對應補 agy | 共用檔 hunk 要小 |
| 23 | 文件：`SPEC.md`、`API.md` | 實作時同步 | — |
| 24 | 搬對話到別台：`scripts/ops/transcript-transfer` | 第二階段（同 #8 的複製問題） | — |

---

## 3. 分階段計畫

### 3.1 MVP：建立／啟動／送 prompt／讀回覆／狀態判讀（單機單身分）
0. **前置（人工）**：①挑一台能跑 agy 的主機（m4p，或把 agm-host 的 VM CPU model 改成含 pclmul／SSE4.2 的）；②登入 spike（§5 第 1 題＋§6 的取樣清單）。
1. **kind 註冊**：`KINDS`、DB CHECK 重建＋升 `SCHEMA_VERSION`、mission／web 列舉、`efforts_for_kind=[]`、模型清單來源。
2. **啟動參數與 pane env**：`--dangerously-skip-permissions`（auto_approve）、`--model`；env：`AGY_CLI_DISABLE_AUTO_UPDATE=true`、（身分的）`HOME`；preflight：找得到 `agy`、能執行（先 `agy --version` 擋掉 SIGILL 主機）、已有憑證（API key 或登入過），否則拒絕啟動並指向登入流程（不要讓 bot 停在登入框）。
3. **settings.json 預寫（merge、原子）**：`trustedWorkspaces`、`statusLine`、`title`；`cache/onboarding.json`（首次條款／色彩頁：**不由 AG Man 代答條款**——見 §5 第 4 題，由使用者在登入 spike 時自己做一次，新身分資料夾複製 `cache/onboarding.json` 與必要設定）。
4. **hook**：`hooks.json` 的 `agents-manager` 具名項（#4），`hookrecv::classify("agy")`，provider＝kind 守衛，`Stop`＋`fullyIdle`→TurnComplete。
5. **狀態信標**：statusLine／title（#6b）→ idle／working／blocked 與 `tool_confirmation_pending`；畫面偵測只當防呆（登入／條款／信任框 → blocked，不代按）。
6. **送達與回覆**：`PreInvocation` 送達證據；回覆從 `transcript_full.jsonl` 讀（先做「剝標籤的純文字」，格式取樣後再補工具步驟）。
7. **persona**：per-bot HOME 的全域 `AGENTS.md`；預設身分走 `injectSteps` 備案；含「禁用內建子代理」。
8. **驗收**：在選定主機上一顆 agy bot 從建立到三問三答（含一次權限確認、一次撞錯誤）、hook 與信標都到；`docs/screenshots/agy-support/`。

### 3.2 第二階段
- resume／`bulk_restart`／`idle_sleep`（`--conversation`／`-c`），換身分複製 `brain/`＋`conversations/`（或放棄）；`/fork`。
- 額度：statusLine `quota` 入庫、web「依模型 bucket」＋每週窗呈現；撞限自動換身分。
- 自動更新：manifest 上游、`cli_update` 安裝（不跑 `agy install`）、header badge。
- 模型：`agy models --output-format json` 清單、`--effort`、`/model` 的替代（重啟帶 `--model`）。
- child agent（herdr shim `--kind agy`、`child_identity`、`pane_identity` 讀 `HOME`）、`agy_transcript.rs` 給沒 hook 的子 agent、`memproc`、`transcript-transfer`、`host_baseline` 檢查 CPU 旗標。
- 登入一鍵化（`login_assist` 驅動 TUI：選 Google OAuth→網址／碼交使用者→`/quit`）。
- 日後評估：`-p --input-format stream-json --output-format stream-json` 的結構化通道（單一行程多回合，免畫面判讀）——需要另一個 bot 傳輸層，不在這一輪。

### 3.3 非目標
- headless／`stream-json` 通道、`remote-control`、`mic-serve`、語音、Antigravity 2.0 GUI 匯出、plugin／skills／MCP 管理、`/rewind`、sandbox 模式、`agy plugin import gemini|claude`。
- 直接呼叫 Google API 讀額度、搬瀏覽器 session／token。

---

## 4. 最重要的整合風險（排序）

1. **「idle」不可信，且 herdr 的 agy 偵測規則已過期**：登入框、色彩頁、條款頁、信任框 herdr 一律 `idle`＋`interactive_ready:true`［實測］，daemon 會把 prompt 打進選單（數字／Enter 會改變選項，條款頁一個 Enter 就切換同意與否）；1.2.16 的權限確認文案已改，manifest 找舊字串（2026-06-24），真正的「等使用者核准」多半被當 idle／working。
   對策：預寫 `trustedWorkspaces`＋複製已完成的 onboarding；**以 statusLine／title 信標的 `agent_state`／`tool_confirmation_pending` 為準**，畫面偵測只擋已知對話框；送 prompt 前確認是空輸入框；登入／條款類永遠只由人處理。
2. **平台、憑證與身分隔離三連問**：agm-host CPU 缺 pclmul → agy 啟動即 SIGILL（glibc／musl 都是）；設定目錄只認 `$HOME`，隔離要換整個 HOME（連帶影響 git／ssh／gh）；憑證放 OS keyring，Mac 的 Keychain 推測不隨 HOME 隔離（未驗），Linux 無 D-Bus 才落檔（位置未知）→ 同機多帳號、搬帳號、SSH 登入各有不確定性。
   對策：preflight 跑 `agy --version`；MVP 先單身分；登入 spike 確認 token 檔位置與 Keychain 行為再決定多身分設計；搬帳號一律走 `/logout`＋重新登入而不是複製檔。
3. **回報管道有洞**：`Stop` 沒有助理文字（要靠 transcript，而 transcript 的模型回覆格式還沒取樣）；`SessionStart` 沒有文件；對話在第一句才建立，沒有預先指定 id 的旗標（resume 對「剛啟動就重啟」的 bot 無 id）；hook 同步卡 agent loop；`hooks.json` 兩種形狀寫錯整檔失效，而且是多工具共用的檔（別人寫壞我們也壞）；hook 與子 agent 共用 pane env。
   對策：以第一個 `PreInvocation` 補身分、`fullyIdle` 守門、payload `conversationId` 守衛、形狀單元測試、merge 只動 `agents-manager` 鍵、hook 亞秒級且永不阻塞。
4. **自我更新與版本漂移**：預設背景更新；官方安裝腳本會改 shell profile 與 purge alias；200 MB binary；一個月內 1.2.4→1.2.16，undocumented 行為（SessionStart、transcript 格式、`agent_state` 值域）隨時變。
   對策：每個 agy 啟動都帶 `AGY_CLI_DISABLE_AUTO_UPDATE=true`（寫進 pane env，涵蓋手動執行）；自己做 manifest 下載＋sha512 驗證，不跑 `agy install`；附錄記驗證過的版本；讀者與信標對未知值寬鬆。
5. **資料、金錢與條款**：首次條款頁預設同意讓 Google 蒐集並使用 Interactions 資料（跑私有 repo／客戶資料前要決定）；`useG1Credits` 會動用個人 credits 續跑（付費超額）——AG Man 不得開；額度是「每週＋依模型 bucket」，與現有 5h／7d 模型不合；Google 對自動化操作個人帳號的條款未查（Gemini CLI 個人版 2026-06-18 已被停服，前車之鑑）。
   對策：由使用者做條款決定（§5）；daemon 啟動前檢查 `useG1Credits`／`enableTelemetry` 並警告；額度只讀 statusLine。

（次要：`bots.kind` CHECK 要重建表；`~/.gemini` 與 Gemini CLI 共用；agy 自帶訊息佇列與 `invoke_subagent`；persona 無 argv 管道；`/model` 是選單；設定檔被 agy 重排版回寫。）

---

## 5. 需要使用者決定的事

1. **要用哪種登入／憑證，哪個帳號**：
   - (a) Google OAuth 個人帳號（可用多模型，免費層每週限額；Keychain／檔案儲存行為待 spike）；
   - (b) GCP 專案／ADC（企業路徑）；
   - (c) `GEMINI_API_KEY`（已驗證不用登入即可跑；按量計費；只有 Gemini 模型；無 keyring 問題，最適合無人值守，但要把 key 放進身分 env／config）。
   建議：MVP 先 (a) 單帳號並做登入 spike；若不想動 OAuth，(c) 是最快能全自動的路。
2. **裝在哪台**：agm-host 目前跑不起來（缺 pclmul）。選項：只裝 m4p；或把 agm-host 的 VM CPU model 改成 `host`（你能改 hypervisor 嗎？）。建議先 m4p。
3. **預設身分要不要動真 HOME 的 `~/.gemini`**：AG Man 要寫 `hooks.json`、`settings.json`（trust／statusLine／title）。選項：①只支援 per-bot 身分（獨立 HOME，真 HOME 不碰）——最乾淨，但 git／ssh 要靠 symlink 補；②預設身分也寫真 HOME（自癒 merge，與使用者手動用的 agy／Gemini CLI 共用同一個 `~/.gemini`）。建議①。
4. **條款頁**：首次啟動預設勾選「允許 Google 使用我的 Interactions 資料」。AG Man bot 會處理這個 repo 與可能的客戶資料——要同意、關掉（`enableTelemetry:false` 與條款頁取消勾選），還是只在不含敏感內容的專案用 agy？AG Man 不會代你回答這頁。
5. **撞限行為**：額度用完要停下來等，還是自動換身分／換模型？`useG1Credits`（付費續跑）是否一律禁止？（建議一律禁止。）
6. **模型預設**：依「模型一律最新版」規則，預設以 `agy models` 當下列出為準（今天最高是 Gemini 3.8 Flash／3.1 Pro；Claude／GPT-OSS 是否在你的方案裡待 spike）。要預設 Pro（`-high`）還是 Flash，與 effort 檔位？
7. **自動更新**：同意 bot 一律關 agy 自己的更新、改走 header 一鍵更新（AG Man 自己下載＋驗 sha512，不跑 `agy install`）嗎？
8. **persona 注入**：接受「per-bot HOME 全域 `AGENTS.md`」為首選、預設身分用 `injectSteps` 首句訊息為備案嗎？
9. **身分代號**：`ag0`／`ag1`…？shell alias 要不要偵測？
10. **要不要把「statusLine／title 指令」當成 AG Man 專用狀態管道**：代價是接管 agy 的狀態列（可 `stack_with_default` 疊在內建之下）與視窗標題。建議要。

---

## 6. 登入後才驗得到的清單（登入 spike 的取樣項目）

在**拋棄式 HOME**登入一個測試帳號，取樣並補回本檔：
1. 登入後 token 存哪（Linux 無 D-Bus 的檔案位置；Mac 是否真進 Keychain、service 名、換 HOME 後是否互相看得到）。
2. `transcript_full.jsonl` 的模型回覆／工具步驟 `type`、`source`、`content`、`tool_calls` 實際樣子；一回合跑多個 tool 時 `Stop` 與 `PostInvocation` 的觸發次數；`terminationReason` 在正常結束時的值。
3. 權限確認框 1.2.16 的完整畫面與 herdr 判定；`tool_confirmation_pending` 何時為 true；撞限（額度用完）時的畫面。
4. statusLine payload 登入後的 `quota`、`email`、`plan_tier`、`cost`、`execution_mode`、`pending_input_count` 實際值；`agent_state` 完整值域。
5. `--dangerously-skip-permissions` 是否也跳過信任框；`trustedWorkspaces` 預寫後是否真的不再跳框（測的是接受後的結果，沒測預寫）；首次色彩頁／條款頁能否靠複製 `cache/onboarding.json`＋`settings.json` 跳過。
6. `AGY_CLI_DISABLE_AUTO_UPDATE=true` 在有新版時是否真的不更新；`agy update` 行為。
7. `SessionStart` 是否在 resume／`/clear` 時再觸發；`PreInvocation` 的 `invocationNum` 是每回合歸零還是對話累計；`injectSteps.userMessage` 是否進對話歷史。
8. 預設 OSC 標題字串（需要的話給 `PLACEHOLDER_TITLES`）；`/fork`、`/clear` 後 `conversation_id` 變化；清草稿鍵。
9. `-c`／`--conversation` 跨 cwd 的行為；對話保留期限。

---

## 附錄：實測摘要（2026-10-04）

- 主機：agm-host（Ubuntu，QEMU Virtual CPU 2.5+，無 pclmulqdq）→ `agy --version` 兩種 build 都 SIGILL；m4p（macOS 27.0.1 arm64）→ 1.2.16 正常。
- m4p 上用獨立 herdr session（測完 `server stop`＋`session delete`，AG Man 既有 session 未動）：`herdr agent start … --kind agy` 成功；未登入畫面 `agent_status:idle`、`interactive_ready:true`；API key 模式走完色彩／條款／信任後進主畫面，`statusLine`／`title` 指令每次狀態變化被呼叫（`agent_state` 依序 `authenticating`→`initializing`→`idle`→`working`→`idle`），herdr 的 `terminal_title` 變成 title 指令的輸出；送一句話後 `SessionStart`／`PreInvocation`／`Stop` 三個 hook 收到上述 payload；`--model gemini-3.1-pro-high`、`--mode plan`、`-c`、離開時的 resume 提示皆如上。
- 清理：m4p 的 pane／workspace／herdr session／拋棄式目錄／`/tmp` 暫存全部刪除；`~/.gemini` 在 m4p 與 agm-host 都不存在；沒有使用瀏覽器。
