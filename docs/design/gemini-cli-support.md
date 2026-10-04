# Gemini CLI 支援：調查與設計（第一階段）

> 狀態：設計稿（2026-10-04）。還沒有任何功能碼。實作完成後，仍有效的決定併進 `docs/SPEC.md`（新的 §「gemini 支援」與附錄「gemini CLI 事實」）、
> `docs/API.md`，這份檔案就刪掉（AGENTS.md：文件只寫現況）。
>
> 調查對象：`@google/gemini-cli` **0.62.0**（npm `latest`，2026-10-03；`preview` 0.63.0-preview.0、`nightly` 0.64.0-nightly）＋
> `@google/gemini-cli-core` 0.62.0。來源：`npm pack` 到拋棄式目錄解開讀 bundle／`dist/src`／`dist/docs`，
> 外加一次在拋棄式 `HOME`＋`GEMINI_CLI_HOME`、**未登入**的 herdr pane 實測（只看到信任框與登入框，真實 `~/.gemini` 沒有被建立）。
> 下文「實測」指這一次；「原始碼」指 0.62.0 bundle；官方文件與原始碼不一致時以原始碼為準（例如 folder trust 預設值，見 G.5）。

## 0. 結論先講

- **可以做，而且有幾處比 grok 順**：gemini 有 per-run 的 hook 注入管道（`GEMINI_CLI_SYSTEM_DEFAULTS_PATH` 指到 bot 自己的 settings 檔，
  hooks 陣列跨層 concat）、可以**啟動時指定 session id**（`--session-id <uuid>`）、可以 `--resume <uuid>`、可以用 `--session-file` 做 fork、
  憑證預設是**檔案**（`<GEMINI_CLI_HOME>/.gemini/oauth_creds.json`，Mac 的 ssh 也讀得到，不像 claude 的 Keychain）。
  herdr 0.9.3 的 `agent start --kind` 已經有 `gemini`（實測：用 `node …/gemini.js` 起的也認得出 `agent: gemini`）。
- **最大的坑**：信任框與登入框出現時 herdr 判 `idle`、OSC 標題也寫 `◇  Ready`（實測），所以「herdr 說 idle 就送 prompt」對 gemini 不安全；
  沒有 `gemini login` 子命令（登入只在 TUI 裡）；沒有 `--append-system-prompt`／`--rules` 類旗標；沒有 reasoning effort 旗標；
  額度是**每日**、依模型層級（Pro／Flash／Flash Lite）分桶，撞限時 TUI 會跳**阻塞式選單**（換 Flash／用 AI Credits／Keep trying／Stop）；
  gemini **預設會自己背景 `npm install -g` 自動更新**。
- `bots.kind` 有 SQLite `CHECK (kind IN ('claude','codex','grok'))`，加 kind 要重建表＋升 `SCHEMA_VERSION`。

---

## 1. Gemini CLI 事實（0.62.0）

### G.1 安裝與執行檔
- npm 套件 `@google/gemini-cli`，`bin: gemini → bundle/gemini.js`，`engines.node >= 20`。主機要有 node（agm-host 有 `~/.local/bin/node` v22.20.0、npm prefix `~/.local/node`，使用者層級、不需 root）。
- 行程名是 `node`，不是 `gemini`（`memproc` 的 LISTED 已含 `node`，但要靠 cmdline 認出是 gemini）。
- 子命令：`mcp`、`extensions`、`skills`、`hooks migrate`（從 Claude Code 搬 hook）、`gemma`。**沒有** `login`／`logout`／`auth status`／`models`／`update`。

### G.2 旗標（`gemini --help`，實測）
| 旗標 | 意義 | AG Man 用途 |
|---|---|---|
| `-m, --model <id>` | 模型（`auto`／`pro`／`flash`／`flash-lite` 或具體 id） | `bots.model` |
| `--approval-mode default\|auto_edit\|yolo\|plan` | 權限模式；`-y/--yolo` 已 deprecated，且與 `--approval-mode` 互斥 | `auto_approve` → `--approval-mode yolo` |
| `--skip-trust` | 這一輪信任 cwd，不跳信任框、不寫 `trustedFolders.json` | 或用 env `GEMINI_CLI_TRUST_WORKSPACE=true`（見 G.5） |
| `-r, --resume <latest\|index\|uuid>` | 接回**這個 cwd 專案**的 session | resume |
| `--session-id <uuid>` | 開**新** session 並指定 id（id 已存在則報錯退出） | 啟動時就知道 native session id |
| `--session-file <path>` | 把一個 session JSONL 匯入成**新** session（新 id 隨機、只留 `user`／`gemini` 訊息、前面插一則 `info`） | fork |
| `--list-sessions`／`--delete-session` | 列／刪本專案的 session | — |
| `-p`／`-i`／位置參數 | 非互動單回合／先跑一句再進互動 | MVP 不用（走 TUI） |
| `--include-directories` | 額外工作區目錄 | persona 候選做法之一（§3.4） |
| `--policy`／`--admin-policy` | 額外 policy 檔 | 分享 cage 類需求日後可用 |
| `--acp` | Agent Client Protocol（stdio JSON-RPC） | 非目標（AG Man 是 TUI-in-herdr 架構） |

`--resume`、`--session-id`、`--session-file` **三者互斥**（原始碼 yargs check）。**沒有** effort／thinking 旗標（thinking 只能用 settings 的 `modelConfigs` 覆寫）。

### G.3 設定目錄與身分隔離
- `GEMINI_CLI_HOME`：**取代「家目錄」**，CLI 在它底下建 `.gemini/`（`utils/paths.js homedir()`）。也就是 `GEMINI_CLI_HOME=~/.gemini-g1` → 檔案在 `~/.gemini-g1/.gemini/…`。
  跟 `CLAUDE_CONFIG_DIR`／`CODEX_HOME`／`GROK_HOME`「直接指設定目錄」語意不同，寫路徑時要多一層 `.gemini`。
- `<home>/.gemini/` 底下（實測＋原始碼）：`settings.json`（user 層）、`oauth_creds.json`、`google_accounts.json`（`{"active": "<email>", "old": [...]}`）、
  `trustedFolders.json`、`projects.json`（cwd → 專案 slug）、`tmp/<slug>/chats/*.jsonl`（session）、`tmp/<slug>/logs.json`、`history/<slug>/`、`state.json`、`installation_id`、`GEMINI.md`（全域記憶）。
- OAuth 憑證：預設**純檔案** `oauth_creds.json`（`fetchCachedCredentialsList`）；只有 `GEMINI_FORCE_ENCRYPTED_FILE_STORAGE=true` 才改走 keychain／加密檔，
  而那條路的 keychain service 名是全域固定的 `gemini-cli-oauth`／`main-account`——**跨 `GEMINI_CLI_HOME` 不隔離**，所以 AG Man 不得設這個變數。
- 其他相關 env：`GEMINI_API_KEY`、`GOOGLE_API_KEY`、`GOOGLE_CLOUD_PROJECT`、`GOOGLE_CLOUD_LOCATION`、`GOOGLE_GENAI_USE_VERTEXAI`、`GOOGLE_APPLICATION_CREDENTIALS`、
  `GEMINI_MODEL`、`GEMINI_CLI_TRUSTED_FOLDERS_PATH`、`GEMINI_CLI_SYSTEM_SETTINGS_PATH`、`GEMINI_CLI_SYSTEM_DEFAULTS_PATH`、`GEMINI_SYSTEM_MD`、`NO_BROWSER`、`CLI_TITLE`（OSC 標題的 context 字）。
- CLI 會從 cwd 往上找 `.env`（到 `.git` 或家目錄為止），再找 `<home>/.env`：專案裡的 `.env` 若有 `GEMINI_API_KEY` 會改變 bot 的認證方式（風險，見 §4）。

### G.4 Settings 層級與 hook
- 優先序（低→高）：預設值 → **system defaults**（`/etc/gemini-cli/system-defaults.json`，可用 `GEMINI_CLI_SYSTEM_DEFAULTS_PATH` 改）→ user → project（`.gemini/settings.json`）→ system（可用 `GEMINI_CLI_SYSTEM_SETTINGS_PATH` 改）→ env → argv。
- `hooks.<Event>` 的 merge strategy 是 **`concat`**（原始碼 schema）：每一層的 hook 都會跑，不會互相蓋掉。
- 事件：`SessionStart`（source `startup|resume|clear`；**啟動就觸發**，不像 grok 延到第一問）、`SessionEnd`、`BeforeAgent`（含 `prompt`）、
  `AfterAgent`（含 `prompt`、`prompt_response`、`stop_hook_active`；一次 agent loop 結束一次）、`BeforeModel`、`AfterModel`（每個串流 chunk 一次，**不要訂**）、
  `BeforeToolSelection`、`BeforeTool`、`AfterTool`、`PreCompress`、`Notification`（`notification_type: "ToolPermission"`）。
- 共通輸入（stdin JSON，snake_case）：`session_id`、`transcript_path`（session JSONL 絕對路徑）、`cwd`、`hook_event_name`、`timestamp`。**沒有** prompt／turn id。
- hook 設定：`{"matcher": "...", "hooks": [{"type": "command", "command": "...", "name": "...", "timeout": <毫秒，預設 60000>}]}`；另有未文件化的 per-hook `env` 物件（`hookRunner` 會併進子行程 env）。
- stdout：空字串＝沒有輸出（`stdout.trim() || stderr.trim()` 為空就不解析）；非 JSON 會被當 `systemMessage` 顯示在 TUI。exit 2＝block（`AfterAgent` 的 exit 2 會**強迫重試**）；其他非 0＝警告。
  → AG Man 的「永遠 exit 0、空 stdout」契約（§4.4）在 gemini 也是對的，而且是必要的。
- **hook 子行程的 env**：`sanitizeEnvironment(process.env, …)`。預設（`security.environmentVariableRedaction` 關）整份 env 照傳；
  使用者若打開 redaction，名字含 `TOKEN`／`KEY`／`AUTH`… 的會被拿掉（`AM_HOOK_TOKEN` 就會不見），但 `GEMINI_CLI_` 開頭的**永遠不濾**；
  有 `GITHUB_SHA` 或 `SURFACE=Github` 時是 strict 模式，只剩白名單（`PATH`、`HOME`…），`AM_*` 全部消失。
- **資料夾不受信任時，所有 merged hooks（含 user／system 層）一律不載入**（`hookRegistry.processHooksFromConfig`：`isTrustedFolder()` 為假就整包跳過）。
  所以 bot 一定要以「受信任」狀態啟動，否則 hook 靜默失效。
- 專案層 hook 有指紋（`trustedHooks`），改了會警告；user／system 層沒有。

### G.5 信任框（folder trust）
- 0.62.0 `security.folderTrust.enabled` **預設 true**（schema `default: true`；官方 `trusted-folders.md` 還寫「預設關」，已過時）。
- 判斷順序（`core/utils/trust.js checkPathTrust`）：`GEMINI_CLI_TRUST_WORKSPACE=false` 或 `GEMINI_RESTRICTED_MODE=true` → 不信任；
  `GEMINI_CLI_TRUST_WORKSPACE=true` → 信任（不寫檔）；feature 關 → 信任；IDE 回報；`trustedFolders.json`（路徑可用 `GEMINI_CLI_TRUSTED_FOLDERS_PATH` 改）。
- 實測畫面（未信任）：框內 `Do you trust the files in this folder?`，選項 `● 1. Trust folder (proj)`／`2. Trust parent folder (gem)`／`3. Don't trust`；
  框上方還有一行 `ℹ Skipping project agents due to untrusted folder…`。**此時 herdr `agent_status = idle`、標題 `◇  Ready (proj)`。**

### G.6 登入
- 沒有登入子命令。第一次開 TUI 且沒有認證時跳 `? Get started / How would you like to authenticate for this project?`，選項
  `1. Sign in with Google`／`2. Use Gemini API Key`／`3. Vertex AI`（實測；herdr 一樣判 `idle`）。
- AuthType：`oauth-personal`、`gemini-api-key`、`vertex-ai`、`cloud-shell`、`compute-default-credentials`、`gateway`；選擇存在 settings `security.auth.selectedType`。
- Google 帳號 OAuth 在無瀏覽器時（`NO_BROWSER=true`）走 **user code**：印 `Please visit the following URL…`，再問 `Enter the authorization code:`，5 分鐘逾時。
  → 符合本 repo「不開瀏覽器、網址＋代碼交給使用者」的登入規則，但要在 TUI 裡做（herdr shell pane 跑 `NO_BROWSER=true gemini`、選 1、讀網址、送回代碼、`/quit`）。
- 已登入判讀（沒有 status 指令）：`<home>/.gemini/google_accounts.json` 的 `active`（帳號 email）＋ `oauth_creds.json` 存在＋ settings 的 `selectedType`。
  API key／Vertex 則看 env。TUI 內有 `/auth login`、`/auth logout`（原始碼 slash command `auth`）；登出走 `/auth logout`，不要自己刪檔。

### G.7 Session（對話紀錄）
- 位置：`<home>/.gemini/tmp/<slug>/chats/session-<YYYY-MM-DDTHH-MM>-<id 前 8 碼>.jsonl`；`<slug>` 來自 `<home>/.gemini/projects.json`
  的 `{ "projects": { "<cwd 絕對路徑>": "<slug>" } }`（實測：slug＝目錄 basename，撞名才加尾碼；舊版是 sha256 目錄，啟動時會搬）。
  hook 的 `transcript_path` 直接給完整路徑，優先用它。subagent 的 session 在 `chats/<父 session id>/<id>.jsonl`。
- 格式（`chatRecordingService`）：append-only JSONL。第一行 metadata `{sessionId, projectHash, startTime, lastUpdated, kind}`；之後每行一則
  `MessageRecord {id, timestamp, type: user|gemini|info|error|warning, content, displayContent?, toolCalls?, thoughts?, tokens?, model?}`；
  **同 id 會再寫一次（以最後一筆為準）**；另有 `{"$set": {...}}`（metadata 更新）與 `{"$rewindTo": "<message id>"}`（`/rewind`）。
  讀者要照「upsert by id＋套用 rewind」重建，不能逐行當新訊息。
- 保留：`general.sessionRetention` 預設 30 天自動刪（最短 1 天）。AG Man 要長留就自己匯入，不能把檔案當永久存檔。
- 每則 `gemini` 訊息帶 `tokens {input, output, cached, thoughts, tool, total}` 與 `model`——可當模型回讀與快取倒數的證據。

### G.8 TUI 畫面
- OSC 標題（`dynamicWindowTitle` 預設 true，`computeTerminalTitle`）：idle `◇  Ready (<ctx>)`、等確認 `✋  Action Required (<ctx>)`、
  背景工作 `⏲  Working…`、回答中 `✦ <thought subject>`；`<ctx>` 是 `CLI_TITLE` env 或資料夾名。**對話框（信任／登入）不會改標題**（仍是 Ready）。
- 輸入框 placeholder：`  Type your message or @path/to/file`；回答中提示 `(esc to cancel, 12s)`；等確認提示 `Waiting for user confirmation...`。
- 權限確認選項含 `Allow once` 等；`auto_edit` 模式顯示 `auto-accept edits`，yolo 顯示 `YOLO mode`。
- 更新橫幅：`Gemini CLI update available! <cur> → <latest>`。
- 撞限（ProQuotaDialog）：`Usage limit reached for <model>.`、`Access resets at <time>`、選項 `Switch to <fallback>`／`Use AI Credits - Continue this request (Overage)`／
  `Manage - View balance…`／`Keep trying`／`Stop - Abort request`／`Upgrade for higher limits`——**阻塞式**，herdr 多半判 blocked 或甚至 idle，要比照 grok 的 `grok_limit_hit_line` 認。

### G.9 額度
- CodeAssist API `retrieveUserQuota` 回 `buckets[] {modelId, remainingFraction, resetTime, tokenType}`；TUI `/stats` 用 `ModelQuotaDisplay`
  依層級（Pro／Flash／Flash Lite）各取最低的 remainingFraction 顯示「Model usage」條與重置時間，文案 `Usage limits span all sessions and reset daily.`
- 也就是：**每日窗、每個模型層級一桶**，不是 5h／7d。

### G.10 自動更新
- `general.enableAutoUpdate` 與 `general.enableAutoUpdateNotification` 預設都是 true；有新版時 TUI 自己在背景跑套件管理器的 global install
  （npx／bunx／binary 安裝不做）。AG Man 的 `cli_update`（使用者確認版本再裝、裝完才重啟）會被它繞過。

---

## 2. 整合點對照表（grok 怎麼做 → gemini 怎麼做 → 風險）

| # | 整合點（主要檔案） | grok 現況 | gemini 設計 | 風險／待驗 |
|---|---|---|---|---|
| 1 | kind 列舉與驗證：`config.rs` `KINDS`／`valid_kind`／`kinds_list`；`db.rs` `bots.kind` CHECK；`mission/api.rs` 三處 `one_of`；`reconcile.rs:425`；`default_session.rs agent_kind`；`update_watch.rs:148`；`web/src/api/types.ts BotKind/BOT_KINDS`；`MissionOptions.tsx`、`missionOpts.ts` | `KINDS = [claude, codex, grok]` | 加 `gemini`。DB 要**重建 `bots` 表**改 CHECK（SQLite 不能改 CHECK）＋ `SCHEMA_HISTORY` 加一行升版 | 重建表要保住 FK／trigger／索引；與別人同時升 schema 要合流（AGENTS.md 規則） |
| 2 | 權限參數：`setup.rs permission_args` | `--always-approve` | `auto_approve` → `--approval-mode yolo`；否則不帶（`default`） | `-y` deprecated 且與 `--approval-mode` 互斥：使用者 `bot.args` 自帶 `-y` 時要避免衝突（daemon 旗標 + bot.args 合併規則） |
| 3 | model／effort：`setup.rs model_args`、`config.rs efforts_for_kind`、`models.rs fetch`、`ModelPicker.tsx` | `-m`、`--reasoning-effort`；`grok models` 讀清單 | `-m <id>`；**effort 不支援**（`efforts_for_kind("gemini") = []`，UI 不給選）。模型清單無 CLI 指令可讀 → 內建清單（`auto`／`pro`／`flash`／`flash-lite`＋ G.2 的具體 id），之後可改讀 bundle 常數 | 清單會過時；`auto` 實際用哪個模型要從 session JSONL 的 `model` 欄回讀 |
| 4 | 啟動前置與 hook 注入：`setup.rs injected_args`、`lifecycle/grok_hook.rs`、`install_remote_grok_hook` | 全域 `<GROK_HOME>/hooks/agents-manager.json`＋env 分派腳本、自癒 merge | **per-run、不碰使用者檔**：daemon 寫 `bots/<id>/gemini-defaults.json`（0600），pane env 設 `GEMINI_CLI_SYSTEM_DEFAULTS_PATH=<該檔>`；內容只有 `hooks`（SessionStart／AfterAgent／BeforeAgent／Notification 指向 hook 指令，`timeout: 5000`）＋必要的 `general.enableAutoUpdate:false`。遠端同理寫到那台的 bot 目錄 | ① 主機若真有 `/etc/gemini-cli/system-defaults.json` 會被我們的檔取代——寫檔前讀它、併進來（或偵測到就退回全域 user 層做法）；② env 會被 bot 底下所有子行程繼承（含 bot 在自己 pane 跑的 `gemini`、herdr 子 agent），見 #6 |
| 5 | 信任：`trust.rs pretrust_for_start`、`tui_prompts::is_grok_trust_dialog`、`prompt.rs:734` | 預寫 `trusted_folders.toml`；畫面上認出信任框才代按 `y` | pane env 設 `GEMINI_CLI_TRUST_WORKSPACE=true`（或 argv `--skip-trust`）：不寫任何檔、也不會跳框；hook 才會載入（G.4）。另寫 `is_gemini_trust_dialog` 只做**偵測→blocked 原因**，不代按 | env 被使用者 `.env` 覆寫不到（env 優先）；但 `GEMINI_RESTRICTED_MODE=true` 會蓋掉，要在 preflight 警告 |
| 6 | 回報管道：`hookrecv::classify`、`hook_cmd`、`hook.sh`（遠端）、§4.1「provider 要等於 bot kind」 | `session_start`→Identity；`stop`+`end_turn`→TurnComplete | `SessionStart`→Identity（`session_id`、`transcript_path`）；`AfterAgent` 且 `stop_hook_active=false`→TurnComplete（`assistant = prompt_response`、`user = prompt`、`turn_id` 用 `timestamp` 或內容 hash）；`BeforeAgent`→送達證據（類 UserPromptSubmit）；`Notification/ToolPermission`→blocked 證據；`SessionEnd` 忽略 | ① **子 agent 繼承 env 誤報**：herdr shim 的 `AM_RESERVED_ENV_KEYS` 要加 `GEMINI_CLI_SYSTEM_DEFAULTS_PATH`、`GEMINI_CLI_HOME`，子 pane 不繼承父的 defaults 檔；再加「hook 的 `session_id` 要等於 run 的 native session」守衛；② 使用者開 env redaction 時 `AM_HOOK_TOKEN` 會被濾 → 分派腳本改讀不含 TOKEN 字樣的 `GEMINI_CLI_AM_*` 或用 per-hook `env`（**token 不得寫進 argv**，#43）；③ `AfterAgent` 在 tool loop 中的觸發時機要實測 |
| 7 | native session id：`runs.native_session_id`、`resume_gate` | 等 SessionStart hook（延到第一問）／`active_sessions.json` | 新對話啟動時 daemon 自己產 UUID 帶 `--session-id <uuid>`，**當場**寫進 run；SessionStart 只核對 | `--session-id` 與 `--resume`／`--session-file` 互斥；id 已存在會 `exit`（重試要換 id） |
| 8 | resume：`start.rs resume_args_by_kind`、`native_resume_plan`、`bulk_restart`、`idle_sleep` | `--resume <id>` | `--resume <uuid>`；限**同一個 cwd**（session 按專案 slug 分）。換身分＝換 `GEMINI_CLI_HOME`：要把 `tmp/<slug>/chats/…jsonl` 複製到新身分，且新身分的 `projects.json` 要有同 cwd 的 slug（比照 claude 的複製，但多一層 slug 對照） | slug 由 gemini 自己分配，新身分下可能不同 → 複製前先讓 gemini 建 slug（或用 `--session-file` 匯入，見 #9） |
| 9 | fork：`start.rs fork_args_by_kind`、`fork.rs` | `--resume <id> --fork-session` | `--session-file <來源 jsonl 絕對路徑>`：需要**路徑**而非 id → `fork_args_by_kind` 要能拿到 transcript 路徑（runs 已存 `transcript_path`）；新 id 由 gemini 隨機產生，靠 SessionStart 回填 | 匯入會丟掉 `info`／`error` 訊息、前面多一則 `Imported session from …`；跨身分 fork 等於免費的「換身分接回」 |
| 10 | 身分隔離：`pane_identity::config_dir_var`、`tools.rs` 身分偵測、`identity_kind`、`IdentitiesPanel.tsx` | `GROK_HOME` | `GEMINI_CLI_HOME`（注意多一層 `.gemini`）。身分名建議 `gm0`／`gm1`…，env `GEMINI_CLI_HOME=~/.gemini-gm1`；預設身分＝真 `~`（`~/.gemini`） | `GEMINI_FORCE_ENCRYPTED_FILE_STORAGE` 會讓所有身分共用 keychain 一格——bot env 與 identity env 都要拒絕這個變數 |
| 11 | 登入／登出／登入探測：`tools.rs login_status_args`、`identity_login_command`、`login_assist/`、`quotaLogin.ts` | `grok login`／`grok logout`／`grok models` | 探測：讀 `<home>/.gemini/google_accounts.json` 的 `active`（帳號）＋ `oauth_creds.json` 存在（不跑 CLI）；登入：host-shell pane 跑 `NO_BROWSER=true GEMINI_CLI_HOME=… gemini`，選 `Sign in with Google`、把網址交給使用者、送回 code、`/quit`；登出：同樣開 TUI 送 `/auth logout` | 登入在 TUI 裡＝要畫面驅動；API key／Vertex 帳號沒有可讀的「帳號名」 |
| 12 | 畫面判讀：`lifecycle/screen.rs`、`delivery.rs`（回音字元）、`poller.rs`、`composer_draft.rs`、`tui_prompts.rs`、`events.rs PLACEHOLDER_TITLES` | `❯ ` 回音、框底模型列、telemetry 橫幅、402／週限橫幅 | 新增：輸入框 placeholder＝空框；`(esc to cancel` ＝ working；信任框／登入框／ProQuotaDialog／權限確認＝blocked（**不能信 herdr 的 idle**）；更新橫幅不阻塞；`PLACEHOLDER_TITLES` 加 `gemini cli`、`◇  ready` 類標題 | 回音字元與回覆標記（`✦`？）要登入後實測；herdr gemini manifest 的 working／blocked 規則未知，要 `herdr agent explain` 實測 |
| 13 | prompt 送達：`delivery.rs`（送字→Enter→驗證）、`owed_delivery`、`send_now`、`queue` | `❯ ` 回音比對；unverified | 有 `BeforeAgent` hook 時以它為送達證據（含原文 `prompt`）；畫面回音當備援 | gemini 對多行貼上／bracketed paste 的行為、`@path` 自動補全會吃字，要實測 |
| 14 | transcript 讀取：`grok_transcript.rs`、`relay_watch.rs`、`poller.rs` fallback、`transcript_origin.rs`、`codex_history.rs` | 沒 hook 的子 agent 讀 `chat_history.jsonl` | 新 `gemini_transcript.rs`：讀 `transcript_path`（或 `projects.json`→slug→`chats/*-<id8>.jsonl`），upsert by id＋`$rewindTo`；`user` 一問、之後第一則無 `toolCalls` 的 `gemini` 是回覆 | 檔案會被 retention 刪；`$set.messages` 可整批改寫 |
| 15 | persona／AG Man 指示：`setup.rs persona_args`、`start.rs`（grok rules file） | `--rules "<讀這個檔>"` | **沒有 argv 管道**。首選：defaults 檔裡另掛一個 `SessionStart` hook，command 為 `cat <bot dir>/gemini-session-context.json`，輸出 `{"hookSpecificOutput":{"additionalContext":"<讀 X 檔並照做>"}}`（文件：interactive 下注入為歷史第一回合）；備案：`--include-directories <bot 專屬 context 目錄>`＋`GEMINI.md`；最後手段：第一則 prompt 前綴 | 首選與 §4.4「hook 一律空 stdout」不衝突（另一支 hook）；resume 時 SessionStart（source=resume）會再注入一次；備案的 `loadMemoryFromIncludeDirectories` 只描述 `/memory reload`，啟動時是否載入要實測 |
| 16 | 子 agent（herdr shim、`child_identity`、§16.6） | 子 agent 用父 env、無 hook，靠 transcript | 同 grok：子 agent 不繼承父的 defaults 檔與 hook token（#6①），走 transcript；身分從 `ps eww` 讀 `GEMINI_CLI_HOME` | herdr 0.9.3 `integration install` **沒有** gemini（只有 agent kind），所以沒有官方 SessionStart 回報 |
| 17 | 額度：`quota.rs`、新 `quota_gemini.rs`、`QuotaStrip.tsx`、`store.ts` 撞限警告、`turn_error.rs` | `/usage` 探測（am-quota session）、週窗 | 窗是**每日、按模型層級**：MVP 只做撞限（畫面 `Usage limit reached for <model>`＋`Access resets at`）標「daily」窗；第二階段 `/stats` 探測（照 grok 的拋棄式 workspace）或直接呼叫 CodeAssist `retrieveUserQuota`（要拿 OAuth token，風險較高，不建議） | 現有 quota 模型是 5h／7d／F 三格，要加「日」與「每層級一條」；ProQuotaDialog 預設游標位置不明，絕不能讓 daemon 誤按 `Use AI Credits`（會花錢） |
| 18 | CLI 版本與更新：`upstream_update.rs`（npm latest）、`cli_update.rs`、`update_watch.rs`、`tools.rs` 安裝說明、`host_baseline/`、web `*UpdateChip` | grok installer＋stable URL | 上游：`https://registry.npmjs.org/@google/gemini-cli/latest`（同 claude 的 `npm_latest`）；安裝：`npm i -g @google/gemini-cli@<target>`（使用者層 prefix）；bot 一律在 defaults 檔關 `general.enableAutoUpdate`（只留通知或也關） | 使用者自己的 user settings 若明寫 `enableAutoUpdate:true` 會蓋過 defaults 層 → 要改用 system settings 層（最高優先）或接受；npm global 裝到哪裡要跟 `tools::detect` 找到的路徑一致 |
| 19 | 快取倒數：`cache_clock.rs ttl_secs` | grok `None` | gemini 先 `None`（implicit caching TTL 未公開）；第二階段可用 JSONL `tokens.cached` 量測後再決定 | — |
| 20 | 其他 kind 分支：`turn_error.rs`（撞限記帳）、`slash.rs`（`/model` 當場套用、`/login`）、`composer_draft.rs`（清草稿鍵）、`memproc.rs`、`events.rs:623`、`api.rs:2651`（哪些欄位要重啟）、`judge.rs`、`release_triage` | 各有 grok 分支 | `/model <id>` 可當場切（要實測指令語法）；清草稿鍵先用 `ctrl+c`（空框的 ctrl+c 會出 `Press Ctrl+C again to exit.`，再按一次就退出——要先確認框非空）；`release_triage::KINDS` 不加 | ctrl+c 退出風險；`/model` 在 gemini 是開選單還是直接切要實測 |
| 21 | 測試隔離：`test_home.rs`、`home.rs` | 測試 HOME 是假的 | 新程式寫 gemini 檔一律走 `crate::home::dir()`；測試行程另外 `remove_var("GEMINI_CLI_HOME")`、`GEMINI_CLI_SYSTEM_*`，免得跑測試那台的 env 指到真目錄 | — |
| 22 | web：`types.ts`、`kindMeta.ts`、`KindTag.tsx`、`kindTag.css`（`--kind-gemini` 兩套主題色）、`IdentitiesPanel.tsx`、`ModelPicker.tsx`、`QuotaStrip.tsx`、`MergedUpdateChip.tsx`、`mock.ts` | 各有 grok 分支 | 對應補 gemini；ModelPicker 不顯示 effort | 共用檔 hunk 要小（AGENTS.md） |
| 23 | 文件：`SPEC.md`（§2、§4.1、§4.4、§6.5i、§12 同級新章、§14、§16、附錄）、`API.md`（kind 列舉、models、quota、identities） | — | 實作時同步 | — |
| 24 | 搬對話到別台主機：`scripts/ops/transcript-transfer` | 搬 `~/.grok/sessions/<cwd>/<id>/` | 搬 `tmp/<slug>/chats/…jsonl`；目標要先有同 cwd 的 slug（或用 `--session-file` 匯入） | 第二階段 |

---

## 3. 分階段計畫

### 3.1 MVP（第一個可用版本）：建立／啟動／送 prompt／讀回覆／狀態判讀
1. **kind 註冊**：`KINDS`、DB CHECK 重建＋升 `SCHEMA_VERSION`、mission／web 列舉、`efforts_for_kind = []`、內建模型清單。
2. **啟動參數**：`--approval-mode yolo`（auto_approve）、`-m`、`--session-id <daemon 產的 uuid>`；pane env：`GEMINI_CLI_TRUST_WORKSPACE=true`、
   `GEMINI_CLI_SYSTEM_DEFAULTS_PATH=<bot dir>/gemini-defaults.json`、`CLI_TITLE=<bot 名>`、身分的 `GEMINI_CLI_HOME`。
   preflight：`kind_probe` 找得到 `gemini`；`google_accounts.json`／API key 至少一個在，否則拒絕啟動並指向登入流程（不要讓 bot 停在登入框）。
3. **hook**：defaults 檔掛 `SessionStart`／`BeforeAgent`／`AfterAgent`／`Notification`，指向既有 `agents-managerd hook gemini --bot … --port …`
   （本機）／`hook.sh gemini <bot> -`（遠端）；`hookrecv::classify("gemini")`；provider＝kind 守衛；子 agent 不繼承（shim reserved keys）。
4. **persona**：SessionStart `additionalContext` 那支專用 hook（§2 #15 首選）；做不到就先用第一則 prompt 前綴並在 SPEC 記下。
5. **畫面判讀**：`screen.rs`／`tui_prompts.rs` 加 gemini 的信任框、登入框、ProQuotaDialog、權限確認 → blocked（附原因），**不代按任何鍵**；
   空框／`esc to cancel` 判 idle／working；撞限寫 `turn_error`（daily 窗，保底 24 小時或 `Access resets at`）。
6. **送達與回覆**：`BeforeAgent` 當送達證據、`AfterAgent.prompt_response` 當回覆；畫面備援先只做「去掉 chrome 的純文字」，不追求完美。
7. **驗收**（要使用者先決定 §5 的帳號）：本機一顆 gemini bot 從建立到三問三答、hook 都到；`herdr agent explain` 截下 gemini 的 working／blocked 判定；
   截 `docs/screenshots/gemini-support/`。

### 3.2 第二階段
- **transcript 匯入**（`gemini_transcript.rs`）：沒 hook 的子 agent、hook 漏掉的回合；upsert by id＋`$rewindTo`。
- **resume**：`--resume <uuid>`；換身分時複製 JSONL＋處理 slug（或改走 `--session-file`）。`bulk_restart`、`idle_sleep`、`resume_gate` 納入 gemini。
- **fork**：`--session-file <path>`，`fork_args_by_kind` 改吃 transcript 路徑。
- **額度**：`quota_gemini.rs` 用拋棄式 workspace 跑 `/stats` 解析「Model usage」各層級的百分比與重置時間；web 額度格加「日」窗。
- **自動更新**：`upstream_update`（npm latest）＋ `cli_update` 安裝指令＋ header badge；bot 關掉 gemini 自己的 auto update。
- **child agent**：herdr shim 認 `--kind gemini`、`child_identity`、`pane_identity` 讀 `GEMINI_CLI_HOME`。
- **登入一鍵化**：`login_assist` 的 TUI 驅動（`NO_BROWSER=true`、user code 交給使用者）。
- 其他：`/model` 當場套用、`cache_clock`、`memproc`、`transcript-transfer`、`host_baseline` 檢查 node ≥ 20。

### 3.3 非目標
- `--acp` 結構化協定、`-p` headless 模式（AG Man 的 bot 一律是 TUI）。
- gemini 的 `/rewind`、checkpoint（`/resume save`）、extensions／skills 管理。
- 直接呼叫 CodeAssist API 讀額度（要動 OAuth token）。

---

## 4. 最重要的整合風險（排序）

1. **herdr 對 gemini 的對話框判 `idle`**（信任框、登入框實測都是 idle＋標題 `◇  Ready`）：daemon 會把 prompt 打進選單，數字鍵甚至會選到選項。
   對策：env 讓信任框不出現、preflight 擋未登入、畫面上認出這些框一律當 blocked；送 prompt 前檢查畫面是不是空輸入框。
2. **hook 靜默失效**：資料夾不受信任時連 user／system 層 hook 都不載；使用者開 env redaction 時 `AM_HOOK_TOKEN` 被濾；`GITHUB_SHA` 存在時 `AM_*` 全濾。
   對策：一律 `GEMINI_CLI_TRUST_WORKSPACE=true`；分派改用不會被濾的變數名或 per-hook `env`；hook 沒到時要有 transcript 備援（第二階段前至少有畫面備援）。
3. **env 繼承造成錯認**：`GEMINI_CLI_SYSTEM_DEFAULTS_PATH`、`AM_BOT_ID` 會被 bot 底下所有子行程繼承；bot 在自己 pane 跑 gemini 或開 gemini 子 agent，
   會用父 bot 的 hook 回報。對策：shim reserved keys、hook 的 `session_id` 必須等於 run 的 native session（我們用 `--session-id` 預先知道）。
4. **撞限是阻塞選單且有付費選項**（`Use AI Credits - Continue this request (Overage)`）：daemon 絕不能對這個框送 Enter／數字；
   額度模型是每日＋按模型層級，跟現有 5h／7d 的 quota 格與自動換身分邏輯不合。
5. **gemini 自動更新與 schema 變動**：預設背景 `npm install -g`，會讓同主機的 bot 版本在 AG Man 不知情下漂移；session 檔近期才從整份 JSON 改成 append-only JSONL（原始碼仍留著把舊 `.json` 轉成 `.jsonl` 的相容碼），
   版本漂移會直接打壞 transcript 讀者與畫面判讀。對策：bot 一律關 auto update、走 AG Man 的 `cli_update`；讀者對未知記錄型別寬鬆略過；附錄記下驗證過的版本。

（次要：`bots.kind` CHECK 要重建表；`GEMINI_CLI_HOME` 多一層 `.gemini` 容易寫錯路徑；沒有 effort；persona 沒有 argv 管道；專案 `.env` 可能偷換認證方式。）

---

## 5. 需要使用者決定的事

1. **登入方式與帳號**：
   - (a) Google 帳號 OAuth（`Sign in with Google`；個人帳號走免費／Google One AI 額度，或 Code Assist 授權）——要哪個 Google 帳號？幾個身分（`gm0`、`gm1`…）？
   - (b) Gemini API key（`GEMINI_API_KEY`，按量計費，無每日上限問題，但要把 key 放進 identity env／config）；
   - (c) Vertex AI（GCP 專案＋ADC／服務帳號）。
   建議 MVP 先 (a) 一個帳號；登入照本 repo 規則用 `NO_BROWSER=true`（網址＋代碼交給你，手機可完成）。
2. **裝在哪台**：只裝 agm-host，還是 m4p 也裝？（需要 node ≥ 20；npm global 裝在使用者層。m4p 若要跑遠端 gemini bot，憑證是檔案、ssh 讀得到，不受 Keychain 限制。）
3. **預設模型**：`auto`（gemini 自己挑 Pro／Flash）、`pro`（目前解析為 Gemini 3.x Pro preview 或 2.5 Pro，視帳號 preview 權限）、還是指定具體 id（`gemini-3.1-pro-preview`／`gemini-3.5-flash`…）？
   依「模型一律最新版」的規則，建議預設 `auto` 或最新 Pro，由 `GET /api/models?kind=gemini` 列出為準。
4. **撞限時的行為**：Pro 用完要不要讓 bot 自動 `Switch to <fallback>`（降級 Flash 繼續做），還是停下來等使用者？`Use AI Credits`（付費超額）是否一律禁止？
5. **自動更新**：同意 bot 一律關閉 gemini 自己的 auto update、改由 header 一鍵更新嗎？
6. **persona 注入方式**：接受「SessionStart hook 注入 additionalContext（會出現在對話歷史第一回合）」嗎？還是偏好 GEMINI.md 檔案方式？
7. **身分代號**：沿用 `ccN` 那種短代號，gemini 用 `gm0`／`gm1`？shell alias 要不要比照 claude 的 `ccN` 一起偵測？
