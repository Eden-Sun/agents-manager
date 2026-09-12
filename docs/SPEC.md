# Agents Manager 規格書（v4.3）

> 修訂紀錄
> - v4.4（2026-09-10）：身份登入探測 fail loud：`hosts[].identities.<name>` 的 `logged_in: null` 帶 `reason`；新增主機層身份登入端點，使用臨時 host-shell pane 帶 identity env 執行 kind 對應登入指令，完成後重新探測並清理 pane。
> - v4.2（2026-09-07）：新增 §17.1「預設提示從哪來」：`GET /api/models` 新增 `identity=` 參數，claude 的 `default_effort` 改讀那個身份的 `settings.json`（`effortLevel` 全域 + `modelSettings.<真實 id>.effortLevel` per-model 覆寫，以 alias 子字串比對），兩者都沒有時落回 claude 官方文件記載、並經真機驗證的內建預設 `high`（初版誤回 `null`，已修正）；UI 的「預設」按鈕與 tooltip 因此顯示真正會生效的強度而不是空話；模型快取 key 多一段身份。
> - v4.1（2026-09-06）：新增 §17「claude 的 `--effort`」：claude 2.1+ 的五級強度（low…max）納入 `bot.effort`，啟動注入 `--effort`，`GET /api/models` 的 claude 清單帶同一組 `efforts`；執行中改強度走 TUI 的 `/effort <level>` 當場套用（帶參數才行，不帶參數是拉桿；claude 會順手存成該帳號的預設）。
> - v4.1（2026-09-06）：新增 §16「從 shell 認出來的身份 cc0～cc6」：daemon 在工具偵測時順便讀該主機登入 shell 的 `ccN` alias（`~/.zshrc` 為退路），只取 `CLAUDE_CONFIG_DIR`、不取旗標，結果放在 `hosts[].shell_identities` 與 `hosts[].identities.<name>.source/config_dir`，**不寫回 config.toml**；bot 啟動、identity 驗證、額度探測與 UI 選單一律改用 `identities_for_host(host)`（config 優先）；額度探測跳過「statusLine 剛更新過」與「這台沒登入」的身份。
> - v3.9（2026-09-06）：新增 §14「每台主機各自的額度」：quota map key 加上 `<host>/` 前綴、`quota.host` 欄位；codex RPC / claude statusLine / claude 與 grok 的 `/usage` 探測全部跟著主機走（遠端探測借 daemon 在該主機的 named session）；三個 poller 改為對 `local` + 每台已連線遠端各跑一輪、`probe_lock` 改 per host；`GET /api/quota` 新增 `host=`、WS `quota_updated` 多帶 `host`；標題列額度條一次只顯示一台並在遠端掛主機名牌。
> - v3.6（2026-09-06）：新增 §12「grok 支援」（第三種 kind：xAI grok CLI 1.0.13）與附錄 F（grok 實測：CLI 旗標、hook 機制與 payload、終端標記、身份隔離）。grok 無每次啟動的 hook 注入旗標，改由 daemon 寫入 `<GROK_HOME>/hooks/agents-manager.json` + 以 pane env `AM_BOT_ID` / `AM_HOOK_TOKEN` 分派的固定腳本；pane env 新增 `AM_HOOK_TOKEN`；`bots.kind` CHECK 重建加入 `grok`。
> - v0（2026-09-05）：初稿。
> - v1（2026-09-05）：依 Codex（gpt-6-astra）第一輪審視與本機實測修訂：回覆來源改為 hooks / notify 為主、終端輸出為備援；資料模型拆出 Bot / Run / Conversation / Turn；socket 契約回填正文；補對帳、ownership、送訊息交易語義、本機存取驗證。
> - v2（2026-09-05）：Codex 第二輪（因用量上限中斷，僅取得初步結論）：hook 身分改 per-bot、hook 早於 RPC 回應、備援與晚到 hook 配對、SQLite schema 與里程碑草案。
> - v3.6（2026-09-06）：新增 §13 專案群組聊天：一個 Project = 一個群組，`@<bot>` / `@all` fan-out 給成員 bot（每個收件 bot 各自 Turn，冪等鍵 `<crid>:<bot_id>`），`messages.group_id` 串起同一次發言；`GET /projects/:id/messages`（跨 bot 合併分頁）、`POST /projects/:id/chat`（略過不可送的 bot、不自動啟動、無 mention → 400 `no_mention`）；daemon 支援 `AM_DATA_DIR`。
> - v3.3（2026-09-06）：§2 Bot 新增可選欄位 `model`（claude `--model`／codex `-m`，注入順序：daemon 旗標 → model → identity.args → bot.args）；§7.2 `PATCH /bots/:id` 擴充為 `{name?, model?, args?, autostart?, auto_approve?, inject_hooks?, identity?, env?}` 並回 `{needs_restart}`（只有改 `name` 需要無 active Run，其餘可線上改、重啟後生效）；新增 `POST /bots/:id/restart`（stop 再 start）；§6.4 DELETE Bot 補上「刪除本機／遠端 `~/.config/agents-manager/bots/<id>/`」；§6.3.7 stall watchdog 的系統訊息改為中性敘述並原樣引用畫面關鍵行。
> - v3.8（2026-09-06）：herdr agent name 改為 `<project slug>-<bot id 尾 6 碼>`，bot `name` 成為可隨時修改的暱稱（不需重啟、允許 CJK，禁空白與 `@ , : ;`）；mention 解析支援 unicode 暱稱與全形標點；新增 `effort`（grok `--reasoning-effort`）；啟動前 preflight 執行檔；群組送出去除 mention；UI 表單精簡（自動核准恆開、無 autostart、新增即啟動、身份只對 claude 且為選項列、grok 顯示強度不顯示模型）。
> - v3.5（2026-09-06）：herdr agent name 改為 `<project label slug>-<bot name>`（§2 表、§6.2.5）；`runs.agent_name` 記錄實際啟動名稱，舊的裸名稱 run 由對帳沿用；bot 名稱唯一性改為專案內。
> - v3.4（2026-09-06）：§11.3.1 遠端 herdr 改由 launchd GUI 網域 LaunchAgent 啟動（Keychain 可用、KeepAlive）；附錄 E 補 Keychain 實測。
> - v3.2（2026-09-06）：§6.3 新增 prompt-stall watchdog（送達後 12 秒內 agent 未離開 idle → Turn `failed` + 系統訊息，畫面含 Not logged in / usage limit 時給明確原因）；§4.3 備援擷取無回覆標記時改為清理後的畫面（去 banner / 狀態列，保留 `⎿` 工具結果行）。
> - v3.1（2026-09-06）：新增 §11 遠端主機（透過 SSH 連遠端 herdr）、`auto_approve`、目錄選擇器、Markdown 渲染；附錄 E 遠端實測。
> - v3（2026-09-05）：依 Grok（grok CLI 1.0.13）第二輪完整審視修訂 15 條：Turn 狀態收斂為 `in_flight`（每 Run 至多一筆，hook 只配那一筆）；per-bot 鎖；active Run 部分唯一索引；先寫 Run 再建 pane；blocked 不觸發備援；hook 子命令最低契約；事件訂閱拓撲實測定案；TOML / SQLite 權威劃分；多項 demo 範圍縮減標為第二階段。

## 1. 目標

一個本機執行的「多 agent 管理器」。使用者透過 Web UI 以聊天方式管理多個正在終端中執行的 coding agent CLI，第一階段支援 **Claude Code** 與 **Codex**；v3.6 起加入 **grok**（xAI grok CLI，§12）。

所有 agent 由 **herdr**（terminal workspace manager，實測版本 0.8.2，socket API protocol 20）承載。本系統不直接 spawn agent 程序，而是透過 herdr 的 Unix socket API 建立 pane、啟動 agent、送訊息、讀輸出、訂閱狀態事件。

第一階段的完成定義（demo）：
1. daemon 啟動後依設定檔在 herdr 專用 session 內建好專案 workspace，並可從 UI 啟動 / 停止 bot。
2. UI 左側依目錄列出 bot 與即時狀態；右側可對 bot 發送訊息並看到回覆氣泡（來源為 hook）。
3. bot 進入 `blocked` 時 UI 顯示終端畫面並可送按鍵。
4. daemon 重啟後能對帳既有 herdr 實例，不重複啟動。

## 2. 名詞與資料模型

| 概念 | 說明 | 主鍵 | herdr 對應 |
|---|---|---|---|
| **Project** | 以目錄為單位的分組。目錄路徑正規化（canonical path）後唯一 | `project_id`（ASCII `[A-Za-z0-9_-]{1,64}`；缺省時由 daemon 產生 ULID） | 一個 `workspace`（本系統建立並記錄 `workspace_id`；對帳發現不存在則設 NULL 並於下次啟動 Bot 時重建） |
| **Bot** | 使用者定義的 agent 設定：名稱、kind、`model`（v3.3，可為空）、啟動參數。屬於一個 Project | `bot_id`（ASCII `[A-Za-z0-9_-]{1,64}`；缺省時由 daemon 產生 ULID，永久） | 無直接對應 |
| **Run** | Bot 的一次執行實例。**每個 Bot 同時最多一個 active Run（DB 部分唯一索引保證）**。欄位含 `agent_name`（v3.5：實際使用的 herdr agent name）、`native_session_id`、`transcript_path`（由 hook 回填，可為 NULL） | `run_id`（ULID） | `pane_id` + herdr agent `name`（v3.8 起 = `agent_name(project.label, bot.id)` = `<label slug>-<id 尾 6 碼>`，與暱稱無關；v3.5 的 `<label>-<name>` 與更早的裸 `bot.name` 由對帳沿用直到重啟） |
| **Conversation** | 使用者與 Bot 的訊息串。**與 Bot 1:1，跨 Run 延續，第一階段永不拆分** | `conversation_id` | 無 |
| **Turn** | 一次「prompt → 回覆完成」的回合。**每個 active Run 同時最多一筆 in-flight Turn** | `turn_id` | Claude `prompt_id` / Codex `turn-id` |
| **Message** | 對話中的一則訊息，`role ∈ {user, assistant, system}` | `message_id` | 無 |

### 2.1 Turn 狀態機

```
status:   in_flight ──► completed            （hook 配對成功）
              │    └──► completed_fallback   （終端備援；之後不再被 hook 覆蓋，UI 標示可能不完整）
              └───────► failed               （agent_blocked / interrupt / stop / 使用者標記放棄）
delivery: pending → ok | unknown | failed    （agent.prompt RPC 的結果，獨立於 status）
origin:   web | external                     （external = 非本系統送出、由 hook 得知的回合）
```

- 「進行中」的定義：`status = in_flight`（不論 delivery）。`completed_fallback` **不算**進行中。
- `delivery = unknown` 時該 Bot **禁止再送 prompt**，只允許 `interrupt`、`stop` 或 `POST /turns/:id/abandon`（標 `failed`），避免下一則 hook 配錯。

### 2.2 Bot 狀態（UI 呈現）

| 面向 | 值 | 來源 |
|---|---|---|
| 連線 | `connected` / `disconnected`（daemon 與 herdr socket） | daemon |
| Run 生命週期 | `stopped` / `starting` / `running` / `stopping` / `exited` | daemon 狀態機 |
| Agent 狀態 | `idle` / `working` / `blocked` / `unknown` | herdr `AgentStatus`；`done` 對應為 `idle` |

合成燈號：disconnected → 灰；stopped / exited → 離線；starting → 黃閃；stopping → 黃；running+idle → 綠；running+working → 藍動畫；running+blocked → 紅；running+unknown → 灰黃。未讀點（第一階段可硬編碼為 0）。

## 3. 架構

```
┌──────────────┐  REST + WebSocket   ┌────────────────────────┐  Unix socket (JSON lines)  ┌─────────────────────┐
│ React 前端    │ ◄────────────────► │ Rust daemon (axum)      │ ◄────────────────────────► │ herdr headless server│
│ (Vite)       │                     │  herdr client           │                            │ session=agents-mgr   │
└──────────────┘                     │  registry + state       │                            └──────────┬──────────┘
                                     │  conversation store     │      hook / notify HTTP               │ panes
                                     │  hook receiver  ◄───────┼───────────────────────────────┐ ┌────▼────────────┐
                                     └────────┬────────────────┘                               └─┤ claude / codex  │
                                              │ SQLite + config.toml                             └─────────────────┘
                                     ~/.config/agents-manager/
```

### 3.1 Rust daemon（`agents-managerd`）

- axum + tokio + serde；SQLite 以 **sqlx**（每個連線 `PRAGMA foreign_keys=ON; PRAGMA journal_mode=WAL`）。
- **權威劃分**：TOML 是 Project / Bot **期望設定**的唯一權威；SQLite 保存 Run / Turn / Message / Conversation / hook token / workspace 映射。daemon 啟動與每次 TOML 寫回後執行 **TOML→SQLite 投影**（依 id upsert；TOML 移除的 Bot 在 DB 標 `deleted_at`，保留歷史）。
- **herdr client**：
  - Endpoint：`~/.config/herdr/sessions/<session>/herdr.sock`。
  - 每個 RPC 開一條新連線：送一行 `{"id":"<string>","method","params"}`，讀一行回應後伺服器關閉連線。
  - 事件訂閱：長連線，`events.subscribe` 一次可帶多個 filter。**拓撲（實測定案）**：
    - 一條**全域**連線：`pane.exited`、`pane.closed`、`workspace.closed`、`pane.agent_detected`。
    - **每個 active Run 一條**連線：`{"type":"pane.agent_status_changed","pane_id":<run.pane_id>}`（此訂閱必須帶 pane_id）。Run 結束時關閉。
    - 事件行格式 `{"event":"<name>","data":{...}}`；名稱**不一致**：`pane.agent_status_changed` 用點號，`pane_updated` / `pane_agent_detected` 用底線，client 以字串比對兩種寫法。
    - 任一連線斷線 → 指數退避重連 → 重連後執行對帳（§6.5）。
  - 連線後 `ping`，`protocol != 20` 記錄警告但繼續。
  - 型別：手寫 M1–M5 用到的 method 子集；未知欄位與未知事件容忍。`docs/herdr-schema.json` 為契約參考。
- **session 管理**：啟動時若 socket 不可連，spawn `herdr --session <name> server`（detached，stdout/stderr 導向 log），輪詢 socket 最多 10 秒。daemon 退出不停 herdr server。
- **per-bot 鎖**：每個 `bot_id` 一把 `tokio::sync::Mutex`，start / stop / prompt / hook 配對 / spool 重放 / 對帳都在鎖內執行。
- **hook receiver**：`POST /hook/claude|codex|grok`，驗 per-bot token → **入佇列後立即回 200** → 背景配對（§6.7）。
- **自動關掉滿意度問卷**：Claude Code 的 `How is Claude doing this session?`（`1: Bad  2: Fine  3: Good  0: Dismiss`）
  會讓 agent 停下來等人回答，卻跟工作無關 → daemon 認出畫面後一律送 `0`（`tui_prompts`）。
  進 `blocked` 的事件當下看一次，另每 10 秒巡邏 `blocked` / `idle` 的 Run；額度探測 pane 同一套判斷
  （那裡對對話框按的 Enter 會變成替使用者評分）。其他等人回答的畫面一概不動。
- **把 claude 的更新通知撈上來**（2026-09-08）：claude 自動更新後只在 pane 最底下那行印
  `✔ Update installed · Restart to update`，不是事件、不會消失、也不擋回合——使用者要點進終端
  才看得到。`update_watch` 每 30 秒對每個 `state=running` 的 **claude** run 做一次
  `pane.read visible 80`，認到就寫進 `runs.update_notice` 並推 `bot_status`，畫面上沒有了
  就清回 NULL；讀不到畫面則跳過不清。不限 `idle`：那句是回合結束時印的，但使用者送出下一句話
  之後 run 就是 `working`，通知還在畫面上也還該看得見。
  認法（`tui_prompts::update_notice`）：`update installed` 與 `restart to update` 兩段都要中，
  **而且只看畫面最下面 6 行非空白的**——那句就印在 statusLine 那一列。光靠兩段字不夠：
  2026-09-08 實測，正在寫這個功能的 agent，畫面正文裡同時引到這兩句，照樣中。
  存在 **run** 而不是 bot，因為等著被套用的更新是這個 claude process 的事，重啟（`POST
  /api/bots/{id}/restart`，也就是套用更新的動作本身）之後的新 run 本來就沒有它。

### 3.2 React 前端

- Vite + React + TypeScript + Zustand。開發期以 Vite proxy 連 daemon；內嵌（rust-embed）為 M8。
- 版面：左側 sidebar 依 Project 分組列出 Bot（狀態燈）；右側為選定 Bot 的聊天視窗。
- 聊天視窗：氣泡（user / assistant / system），底部輸入框；assistant 氣泡顯示來源標籤（hook / terminal-fallback），fallback 標示「可能不完整」。
- `blocked`：顯示終端 `visible` 快照（每 1 秒輪詢），提供按鍵按鈕（Enter / Esc / y / n / ↑ / ↓ / ctrl+c）。
  另**自動彈出全畫面終端**（只針對目前檢視中的 bot，關掉後同一次 blocked 不再彈），視窗內鍵盤直通
  herdr `agent.send_keys`（單一字元與 ctrl/alt/shift 組合），⌘ 系列留給瀏覽器。
- 終端分頁：唯讀文字快照（手動刷新）。不做 xterm.js。
- 設定：新增 Project（目錄）與 Bot（名稱、kind、args、autostart），寫回 TOML。

## 4. 回覆擷取

### 4.1 主要來源：hooks / notify（每次啟動注入，不改使用者全域設定）

**Claude Code**：`--settings <abs path>`，檔案由 daemon 產生於 `~/.config/agents-manager/bots/<bot_id>/claude-settings.json`：

```json
{"hooks":{
  "SessionStart":[{"hooks":[{"type":"command","command":"/abs/agents-managerd hook claude --bot <bot_id> --token <t> --port <port>"}]}],
  "Stop":[{"hooks":[{"type":"command","command":"/abs/agents-managerd hook claude --bot <bot_id> --token <t> --port <port>"}]}]
}}
```

（v3：移除 `Notification` hook，避免無語意事件；`stop_hook_active = true` 的 Stop 忽略。）

實測（Claude Code v2.1.261）stdin：`SessionStart` 含 `session_id`、`transcript_path`、`cwd`；`Stop` 含 `session_id`、`transcript_path`、`prompt_id`、`last_assistant_message`、`stop_hook_active`。

**Codex**：`-c notify=["/abs/agents-managerd","hook","codex","--bot","<bot_id>","--token","<t>","--port","<port>"]`。實測（codex-cli 0.153.4）argv 最後一個參數為 JSON：`{"type":"agent-turn-complete","thread-id","turn-id","cwd","input-messages":[...],"last-assistant-message"}`。
使用者原本的 `notify` 在此實例中被覆蓋；**轉呼叫原 notify 為第二階段**（UI 顯示提示）。

**grok**（v3.6）：無每次啟動注入旗標，改為全域 hooks 檔 + env 分派，見 §12.2。

hook 身分為 **per-bot**（`bot_id` + `bots.hook_token`），daemon 解析該 Bot 目前的 active Run。原因：對帳收養會產生新 `run_id`，但存活的 agent 程序仍持有啟動時的參數。pane env 中的 `AM_RUN_ID` **僅供診斷，不作身分**。

### 4.2 回補來源：transcript（第二階段，先留介面）
- `messages.source` 保留 `transcript` 值；`runs.transcript_path` 由 SessionStart 回填。UI「重新載入完整回覆」按鈕 disabled。

### 4.3 備援來源：終端快照
- 觸發：Turn `in_flight` 且 `delivery = ok`，agent 狀態由 `working` 轉為 **`idle`**（`blocked` 不觸發），5 秒內未收到 hook。
- 執行：CAS `UPDATE turns SET status='completed_fallback' WHERE id=? AND status='in_flight'`；成功才 `agent.read {source: recent_unwrapped, lines: 200}`，取上次游標（`last_read_revision` + 已見文字尾端 hash）之後的內容，依 provider 規則抽回覆（Claude：`⏺ ` 開頭；Codex：`• ` 開頭；grok **無標記**，一律走下一條的 `clean_screen`，§12.3）。
- 無回覆標記時（v3.2）改用 `clean_screen`：取最後一行 prompt 回音之後的內容，去掉 banner、方框、分隔線、狀態列、spinner 與 `⚠` 提示，保留 `⎿` 工具結果行（去掉符號）；仍為空 → 「（終端沒有可辨識的回覆）」。不再把整個畫面塞進氣泡。
- **回音剝除**（v4.2 補強）：畫面上只有 `❯ <第一行>` 那列算回音，多行 prompt 的第 2..n 行會留在畫面上，所以再逐行比對把它們去掉。逐行比對在**極窄的 pane** 下必定失敗——TUI 自己就把文字排成一欄、每列一個字（herdr 的 `recent_unwrapped` 只還原終端軟換行，還原不了 TUI 的排版），於是整個 prompt 曾被當成回覆存起來、在 UI 上直立成一條字柱。因此加了**去空白比對**的後備：把兩邊的所有空白字元拿掉再比，且要求候選文字**開頭**就是 prompt 的一段結尾（至少 8 個字元），候選本身若只是那段結尾的片段就整個是回音。
- **無法辨識就說無法辨識**：剝完是空的 → 「（終端沒有可辨識的回覆）」；剝完仍是一欄單字元（`is_shredded`：≥6 行且 ≥70% 的行只有 1–2 個字）→ 「（終端太窄，輸出被切成單字元而無法辨識；把 herdr 的 pane 拉寬一點就會恢復）」。窄 pane 會把字與字之間的空白吃掉，重組不回來，所以不猜。
- 存為 assistant Message `source = terminal_fallback`、`incomplete = 1`。**之後晚到的 hook 不覆蓋**（去重後丟棄並 log），避免跨回合錯配。
- 第二個觸發點是 `turn_progress` 的輪詢器（§4.3 的狀態沒翻時的安全網：空輸入列且畫面 14 秒沒變）。它**也在 bot 鎖內**呼叫同一支
  `try_fallback`（2026-09-12 review #5：以前在鎖外，與 hook 交錯成同回合兩則 assistant）；沒收成（spinner 殘影、工具還在跑、hook 先到）
  就繼續盯著，不自己退場——Turn 由任何一方收掉時，迴圈開頭的「還在 in_flight 嗎」自然結束它（review 可能 b）。
- **沒有 hook 的 run（v4.2）**：被認領的 pane（`runs.adopted = 1` 且 `bots.inject_hooks = 0`，典型是 bot 自己開的
  子 agent，見 §6.5a）永遠等不到 hook，終端快照對它不是備援而是**唯一來源**。所以這種 run 的 `working → idle`
  若沒有 in-flight Turn，不再什麼都不做，而是用同一份快照補一筆 `origin = external`、`status =
  completed_fallback` 的 Turn：prompt 回音記成 user 訊息，抽出來的回覆記成 `source = terminal_fallback` 的
  assistant 訊息。認領當下 agent 還在 `working` 時則先開一筆 in-flight Turn（等同「看到使用者在 pane 裡打字」），
  回覆照常即時串流、照常由本節收尾。
  - 邊界：沒有游標（`last_read_tail_hash`）時要求畫面上有 prompt 回音，否則整個 scrollback 會被當成一則訊息；
    擷取不到就只推進游標、不寫訊息（沒有 Turn 在等答案，「（終端沒有可辨識的回覆）」只是噪音）。
  - 去重與上限：游標 + 與上一則 assistant 訊息比對（herdr 同一輪可能報兩次 `working → idle`；重啟會再讀到同一
    個畫面），單則上限 6000 字；認領時的補記只在對話**還是空的**時候做一次。

### 4.3a 回合被 API 中斷（v4.4）
claude 的連線在回應中途掉了，pane 上只會多一行

```
⏺ API Error: Connection lost mid-response. The response above may be incomplete.

✻ Baked for 5m 21s · done 12:56 AM
```

然後就收工。**hook 照樣送 Stop、herdr 照樣報 `working → idle`**，於是這個回合被記成 `completed`、
側欄一顆綠燈——使用者以為做完了，實際上回應是斷的（實測 pane `w168:pE`、`w168:p15`，2026-09-09）。

- 觸發：與 §4.3 同一個 `working → idle` 邊、同一次 `agent.read {source: recent_unwrapped}`。備援有沒有
  出手都要跑——斷線的回合正是 hook 會照常送 Stop 的那一種，跑到這裡時回合早就 `completed` 了。
- 判定（`daemon/src/turn_error.rs`）：從畫面**底部往上**掃最多 30 行，剝掉框線與前導記號後，
  碰到的第一個非 chrome 行若以 `API error`（不分大小寫）開頭，或是額度用盡的拒絕
  （`You've reached your Fable limit. Run /usage-credits …`，2026-09-10 加入：claude 0 秒就 `done`、
  一個字都沒回，使用者只看到綠燈）就算命中。chrome 沿用 §4.3 的 `is_noise` / `is_activity_shape`
  （spinner、分隔線、狀態列）加上空的輸入框列與 `Update installed · Restart to update` 那行。
  - 「最後一件事」是關鍵：`API error · Retrying in 0s · attempt 1/10` 之後 agent 又把答案講完了的話，
    橫幅還留在畫面上但下面有回覆——那是重試成功，不算中斷。
- 記錄：那行原文寫進 `runs.turn_error`（在 run 上而不是 bot 上，理由同 `update_notice`：它屬於這個 CLI
  程序，重啟就是新的 run、欄位為 NULL），並在對話裡補一則釘在該回合上的 `system` 訊息（`incomplete = 1`、
  附終端快照）。回合若還是 `in_flight` 就一併收成 `failed`，不然輸入框會一直鎖著。
- 清除：下一個回合一開（`arm_progress`）就把 `turn_error` 設回 NULL 並推 `bot_status`。
- 同一則錯誤只記一次：`runs.turn_error` 已經是那行就直接返回（同一回合會被掃到好幾次）。
- UI：側欄那列一個紅色「⚠ 中斷」記號，標題列一顆紅色 chip「⚠ 回合被中斷（API 錯誤）」，點開看得到原文
  與「重送上一則」（走既有的 `POST /bots/{id}/prompt`，不另開一條路）。見 `docs/UI-DECISIONS.md`。

### 4.4 hook 子命令（`agents-managerd hook claude|codex|grok`）最低契約
1. wall-clock ≤ 3 秒；**永遠 exit 0；永遠空 stdout**（即使錯誤也不印 JSON）。
2. 讀 stdin（Claude、grok）上限 1 MiB，超限截斷並標 `truncated`；Codex 取 argv 最後一個參數。
3. POST `http://127.0.0.1:<port>/hook/<provider>`（寫死 IPv4 loopback；設 `NO_PROXY=127.0.0.1`；連線逾時 300 ms、總逾時 2 秒），header `X-AM-Bot-Token`，body `{bot_id, provider, payload, received_at}`。
4. 失敗 → 以 `O_APPEND` 追加一行 JSON 到 `~/.config/agents-manager/bots/<bot_id>/hook-spool.jsonl`；寫入失敗只記自己的 log 檔（`hook.log`），仍 exit 0。
5. `--port` 來自 command 列（不依賴 env）；env `AM_PORT` 為備援。
6. daemon 端 spool 重放：取得 per-bot 鎖 → rename spool 為 `.replaying` → 逐行依 §6.7 處理 → 刪檔 → 釋放鎖。重放期間該 bot 的 HTTP hook 在鎖外等待（因同一把鎖）。
7. **遠端 bot（v4.3）不走 HTTP**：第 3 點的 POST 換成「寫 spool + `herdr pane report-agent`」，第 4 點的 spool 從備援變成唯一內容通道，觸發重放的是 herdr 狀態事件。完整規則見 §11.4。

### 4.4a 模型／強度／fast：誰決定 runtime，UI 顯示哪一個（v4.4，2026-09-09）

`bots.model` / `bots.effort` / `bots.fast` 是**設定**，不等於那顆 bot 現在真的在跑的東西。三個 kind
差很多：

| kind | 執行中改 | 怎麼套用 |
|---|---|---|
| claude | 可以 | `apply_live_setting` 送 `/model <alias>`、`/effort <level>` 進 TUI；`/model` 在有對話紀錄時會跳「Switch model?」確認框，daemon 送完回頭看畫面、按 `1` 確認，框關掉才算套用，關不掉就 Esc 退出並回 `needs_restart`（2026-09-11） |
| grok | 可以 | 同上（`/model <id> [effort]`、`/effort <level>`） |
| codex | 可以（2026-09-09 補） | `/model` 的兩層選單 ＋ `/fast` 開關，見下面「codex 的即時套用」 |

`PATCH /api/bots/{id}` 只在**真的送不進去**時才回 `needs_restart: true`（agent 在忙、有回合在飛、沒有
pane、選單長得不對、回讀對不上）。**回了 `needs_restart` 之後 UI 不可以直接顯示新設定**——
2026-09-09 的實況就是這樣壞的：AG Man 寫 `gpt-5.6-luna-High`，同一顆 bot 的終端底部 codex 自己印
`gpt-5.6-luna xhigh fast`（process 的 argv 上確實是 `-c model_reasoning_effort="xhigh"`，只是那是上一次
啟動送的）。

規則：

- **daemon 記下 runtime**：`start_inner` 在 `agent.start` 前把最終 argv 用 `models::model_effort_from_argv`
  讀回來，連同 service tier 存進 `runs.runtime_model` / `runtime_effort` / `runtime_fast`。讀 argv 而不是抄
  `bots`，因為 `effort_checked` 會丟掉該模型不收的等級，使用者自己的 `bot.args` 也可能再帶一個 `-m`。
- **slash 指令套用成功就同步改**（claude / grok）：runtime 真的換了，記錄要跟著換。
- **收編的 pane（`adopted`）**啟動時三個欄位都是 NULL＝不知道；UI 這時什麼都不比、也不標。
  codex 例外（2026-09-09 補）：它自己把三個值印在狀態列上，所以 reconcile 會讀那一行把 NULL 補起來
  （`reconcile::fill_codex_runtime`，只補還是 NULL 的）。不補的話 UI 會拿 `bots` 頂上去——實況是
  `bots.fast = false`、終端的狀態列卻寫著 `fast`，正是這一節禁止的「靜靜顯示一個還沒生效的值」；
  而且 `/fast` 是開關，沒人知道的 tier 等於沒人切得掉。
- **UI 一律顯示 runtime**（`ModelTag`、標題列狀態列都是），設定跟 runtime 不一致時多一顆 `⟳`／
  「需重啟」chip，按下去就是 `POST /api/bots/{id}/restart`。**不准**靜靜顯示一個還沒生效的值。

**codex 的 fast 要兩個方向都送**（2026-09-09 修）：`service_tier` 少送一次不等於「不要 fast」，而是
「聽 `~/.codex/config.toml` 的」——而那個檔案常常寫著 `service_tier = "fast"`（TUI 自己的 Fast 開關就寫在
那裡）。結果是沒勾 fast 的 bot 照樣跑在 fast 上，UI 上卻一個字都沒有。所以 codex 一律帶
`-c service_tier="priority"`（勾了）或 `-c service_tier=""`（沒勾）；0.153.4 實測後者會讓狀態列的 `fast`
消失，並印一行 `Configured service tier `` is not advertised … and will be omitted from requests`
（`⚠` 開頭的行本來就會被終端清洗丟掉，不會變成回覆）。`priority` 是 `model/list` 唯一有廣告的 tier，
也就是 TUI 上顯示成 `fast` 的那個。

`model` / `effort` 沒有做同樣的「明講預設」：它們的「不指定」在 UI 上就寫成「使用 CLI 預設」，
CLI 的預設包含使用者的 `config.toml`，這是說得通的；fast 是一顆 on/off，關著就該是真的關著。

**codex 的即時套用**（`daemon/src/codex_live.rs`；0.153.4 在拋棄式 pane 實測）：

- `/model` **不吃參數**。`/model gpt-5.6-sol high` 會被當成一般 prompt 送給模型（浪費一個回合、
  什麼都沒改）。空的 `/model` + Enter 開 `Select Model and Effort` 編號選單 → 按數字選模型 → 立刻
  跳出 `Select Reasoning Level for <model>` → 再按一個數字，codex 印
  `• Model changed to <model> <effort>`。`Max` / `Ultra` 在第一層的 `More reasoning…` 底下再一層。
- `/fast` 是**開關**（`• Service tier set to priority` / `… default`），沒有「設成 X」的形式，所以只有在
  現在的 tier 跟目標不同時才按。優先讀 `runs.runtime_fast`；那一欄是 NULL（收編的 pane）時**改讀狀態列**
  而不是拒絕——狀態列本來就是 runtime 的定義，拒絕等於收編的 codex 永遠只能靠重啟才切得掉 fast。
- **live 欄位的閘門要把 `fast` 一起算進去**（2026-09-09 修）：`PATCH` 判斷「這次只動了可即時套用的欄位」
  的那段（`api.rs` 的 `extras`）漏掉 `fast` 的 skip，於是**只改 fast** 永遠被自己算成「還動了別的」，
  0.017 秒就回 `needs_restart: true`，pane 上一個鍵都沒送。真機驗證（0.153.4，拋棄式 pane）：
  `{"fast":true}` → `• Service tier set to priority`、狀態列多一個 `fast`、`runtime_fast=1`；
  `{"fast":false}` → `• Service tier set to default`、`fast` 消失、`runtime_fast=0`；同值再送一次不會多按
  一次（沒有新的 `Service tier` 行）；`{"model","effort","fast"}` 一起送 5.7 秒內兩件事都做完。
- **選單一律用讀的**：號碼與順序來自帳號的模型清單，`(default)` / `(current)` 標記會跑，所以每一步都
  回讀 pane，比對「號碼後面到兩個空白為止」的 label（第 5 列的說明字串裡有 `Max and Ultra`，
  拿整行比對會按錯那一列）。
- 只改強度也要先選模型（選單就是兩層一起問）：bot 沒指定模型時選 `(current)` 那一列。
- **最後回讀狀態列**（`<model> [<effort>] [fast] · <cwd> · Context …`）確認真的變了；`runs.runtime_*` 存的
  就是這一行讀到的值，不是我們以為送出去的值。對不上就回 `needs_restart`。
- 副作用：codex 跟 claude 一樣會把選擇**存成該帳號的預設**（寫進 `~/.codex/config.toml`）。這是 CLI 的
  行為，不是我們寫的。

## 5. 設定檔

路徑：`~/.config/agents-manager/config.toml`

```toml
[server]
listen = "127.0.0.1:7788"
herdr_session = "agents-manager"

[supervisor]
notify_interval_secs = 600    # 喚醒總管的最短間隔；0 = 每次 controller tick 都喚醒

[[projects]]
id = "01JABC1234567890XYZ1234567" # 必須符合 [A-Za-z0-9_-]{1,64}；缺省時 daemon 首次載入自動補寫
path = "/Users/me/project/foo"
label = "foo"

  [[projects.bots]]
  id = "01JABC1234567890XYZ1234567" # 必須符合 [A-Za-z0-9_-]{1,64}
  name = "foo-claude"        # herdr agent name：[a-z][a-z0-9_-]{0,31}，全域唯一
  kind = "claude"            # claude | codex | grok
  args = ["--model", "opus"] # 原生參數，接在 daemon 注入參數之後
  autostart = true
```

- 載入時未知欄位以 `serde_ignored` 收集並用 `WARN` 回報完整欄位路徑（仍容許載入，保留向前相容）。
- 寫回：第一階段以 serde 全量序列化（註解不保留），先寫暫存檔再原子 rename；daemon 內單一 mutex 序列化；若 mtime 與上次讀取不符，會在同一把 mutex 內重新讀取後再套用更新，重新解析失敗才回錯誤。內容沒變就不碰檔案（no-op 不會把註解與未知欄位洗掉）。`toml_edit` 保留註解為第二階段。
- 改 `name` 時若有 active Run 拒絕（herdr agent name 綁定啟動時的名稱）。
- `[supervisor] notify_interval_secs`（預設 **600**，AGM 運維面的說明見 §18.3）：事件照舊即時寫入 `supervisor_inbox`，**不丟也不延遲入庫**；
  被節流的只有「把未 ack 事件推給總管、喚醒它」這個動作——每 ≥ 這個秒數才推一次，一次把這段期間累積的
  未 ack 事件彙整成同一則 `[AG Man 通知]`。health 偵測（30 秒）、watchdog 與模型控制器 TICK 不受影響。
  總管 busy 時照舊延後，成功送出才開始下一個視窗（送失敗不會吃掉一個視窗）。`0` = 回到節流前的行為。
  上次喚醒時間存在 `supervisors.last_notify_at`，重啟不會多換來一次喚醒。改這個值要重啟 daemon。
- 改 `listen` port 需重啟 daemon，且既有 agent 的 hook 會打舊 port（靠 spool + 對帳補入）；UI 顯示提示。

## 6. 生命週期

### 6.1 daemon 啟動
1. 載入 config，補寫缺少的 id；TOML→SQLite 投影。
2. 確保 herdr session 執行中；`ping`。
3. 對帳（§6.5）。
4. 建立全域事件連線與各 active Run 的狀態連線。
5. 對每個 Bot 重放 spool。
6. `autostart = true` 且無 active Run 的 Bot 執行 §6.2。

### 6.2 啟動 Bot（在 per-bot 鎖內）
1. `INSERT runs (state='starting')`；若違反 active Run 唯一索引 → 回 409 並附既有 `run_id`。
2. 取得或建立 workspace：`projects.workspace_id` 存在且 `workspace.get` 成功 → 用之；否則 `workspace.create {cwd, label, focus:false}` 並更新映射。
3. 啟動前先完成不需要 pane 的準備：產生 hook 注入檔（遠端主機可能在此透過 ssh 上傳）及 CLI 參數。hook 注入／參數準備失敗 → Run `exited`（`ended_at` 填入），**不建立 workspace 的新 tab／pane**。
4. 取得 pane：
   - 若 workspace 剛由本步驟建立 → 用 `root_pane`。
   - 否則呼叫 `tab.create {workspace_id, cwd, label, focus:false, env}`，取回該 tab 的 `root_pane`。`label` 由
     `tab_label(bot)` 產生：使用 `bot.name.trim()`，空字串時為 `"bot"`。因此每個 bot 都有自己的 tab，
     不再與其他 bot 共用同一 tab 的寬度；`acquire_run_pane` 不呼叫 `pane.split` 或 `pane.layout`。
   - **v4.2 以前（歷史註記）**：曾用 `pane.split {target_pane_id: <該 workspace 中面積最大的 pane>, direction, cwd, focus:false, env}`。
     **不是**第一個 pane（v4.2 修正）：一直切第一個會讓它每次減半，實測第 6 個 bot 只剩 **6 欄**，窄到 agent 的 TUI
     把文字排成一欄、每列一個字，終端備援完全讀不出東西（§4.3）。當時改成先 `pane.layout` 取每個 pane 的矩形，挑面積
     最大的那個，沿長邊切——終端字元格高約為寬的兩倍，所以 `width >= height * 2` 才切 `right`，否則切 `down`。這樣長出來的
     是網格而不是階梯：同一個 185×54 視窗開 6 個 bot，舊規則最窄 6 欄，新規則最窄 **46 欄**（實測）。`pane.layout` 失敗時
     退回舊行為。
   - `env`：`AM_BOT_ID`、`AM_RUN_ID`（診斷用）、`AM_PORT`、`AM_HOOK_TOKEN`（v3.6；`inject_hooks = false` 時不給，grok 的分派腳本以此判斷是否回報）、`CLAUDE_CODE_CHILD_SESSION=""`、`CLAUDECODE=""`。
   - 失敗 → Run `exited`（`ended_at` 填入），回 502。
4. 更新 Run 的 `workspace_id` / `pane_id` / `tab_id`。產生 hook 注入檔（Claude）或參數（Codex）。
5. 先寫 `runs.agent_name = agent_name(project.label, bot.name)`，再 `agent.start {name: <agent_name>, kind, pane_id, args: injected ++ bot.args, timeout_ms: 60000}`（立即回傳 `launch_pending`）。之後所有 herdr 目標（wait / prompt / keys / stop）一律用 `run.agent_name`，缺值時退回 `bot.name`。pane 建好後到 `agent.start` 成功前的**任何**失敗 → Run `exited` + 盡力 `pane.close`；若關閉後 tab 為空也盡力 `tab.close`。
6. 開該 pane 的狀態訂閱連線。
7. `agent.wait {until:[idle,done,blocked], timeout_ms: 60000}`：
   - `idle/done` → Run `running`，agent `idle`。
   - `blocked` → Run `running`，agent `blocked`（例如 trust 提示），UI 顯示終端。
   - timeout / error → **不**關 pane；`agent.get` 若有 agent → Run `running` / `unknown`；若無 → Run `exited` + `pane.close`。

#### tab 生命週期

停止 Bot 與對帳回收 orphan pane 共用 `close_pane_and_tab`：先呼叫 `pane.close`，再以 `tab.list {workspace_id}`
確認該 pane 所在的 tab。只有查到該 tab 的 `pane_count == 0` 時才呼叫 `tab.close`；仍有 pane 的共享 tab 一律不動，
避免連鄰居的 agent 一起關掉。tab 已被 herdr 回收時視為已完成；`tab.list` 失敗則不猜測、不關 tab。舊的
one-bot-one-tab 以前建立、沒有 `tab_id` 的 Run 只關 pane，不會因而關閉共享 tab。

正在執行的 bot 可由 `POST /api/bots/{id}/pane/move-to-tab` 呼叫 `move_pane_to_own_tab` 搬遷：若目前 tab
不是該 pane 獨占，就以 bot 的 `tab_label(bot)` 建立新 tab，把 pane 搬過去並更新 Run 的 `tab_id`；pane id、狀態
訂閱與進行中的 Turn 都不變，且不重新啟動 agent。若 pane 已獨占 tab，端點維持原狀；搬遷後原 tab 仍有其他 pane 時也不關閉。

### 6.3 送訊息（在 per-bot 鎖內，單一 DB 交易）
1. 冪等：先查 `client_request_id`；已存在 → 回同一 `turn_id`（200），**命中冪等時不做後續前置檢查**。
   同一個 request 重送時，即使已有新的 Turn 在飛，也回原本的 `turn_id`，不回 409。
2. 前置檢查：Run `running`；agent 狀態 ≠ `blocked`；該 Run 無 `in_flight` Turn；無 `delivery=unknown` 的 Turn
   → 否則 409（body 含原因與既有 `turn_id`）。
3. `INSERT turns (status='in_flight', delivery='pending', origin='web')` + user Message；commit；推 WS。
4. 鎖內呼叫 `agent.prompt {target, text}`（逾時 10 秒）：成功 → `delivery=ok`；`agent_blocked` → `delivery=failed`、`status=failed`；逾時 / 連線錯誤 → `delivery=unknown`（不重送）。
5. 完成靠 hook（§6.7）或備援（§4.3）。
6. `interrupt`（送 `esc`）/ `stop` 時，將 in-flight Turn 標 `failed` 並加 system Message。

> 因 Turn 在送 prompt **之前**已是 `in_flight`，hook 早於 RPC 回應到達也能配對。

7. **stall watchdog（v3.2）**：`delivery = ok` 後啟動 12 秒計時；期間收到該 Run 的 `working` 或 `blocked` 事件即取消。逾時仍 `in_flight` 且 agent 仍 `idle/unknown` → 讀 `visible` 快照判斷原因（`Not logged in` / `usage limit`）→ Turn `failed` + system Message（含原因與快照）。v3.3：訊息改為中性敘述（不再斷言「尚未登入」），原樣引用畫面中含 `Not logged in` / `/login` / `unlock-keychain` / `usage limit` / `limit` 的行，並提示 macOS Keychain 在 ssh 環境可能讀不到。避免 agent 對輸入無反應時回合永遠卡住、輸入框鎖死。

### 6.4 停止 Bot（在 per-bot 鎖內）
- `interrupt`：`agent.send_keys [esc]`，Run 狀態不變。
- `stop`：Run `stopping` → in-flight Turn 標 `failed` → `ctrl+c` ×2（間隔 500 ms）→ 等 `pane.exited` 或 agent 消失最多 10 秒 → 否則 `pane.close` → Run `stopped`（`ended_at`）→ 關閉狀態訂閱。
- DELETE Bot：先 stop → TOML 移除 → DB `deleted_at`，保留 Conversation 與訊息 → 刪除該 bot 的 `~/.config/agents-manager/bots/<bot_id>/`（遠端 host 以 ssh `rm -rf`，失敗只 log）（v3.3）。
  `managed_by = 'team'` 的成員不走這條：回 409 `team_managed`，由 team（退役／換成員／刪 team）處理（2026-09-12 review #3）；
  `child` 不進 TOML，直接停 pane 並 `deleted_at`。
- 重啟 Bot（v3.3）：`POST /bots/:id/restart` = 有 Run 就先 stop 再 start，用來套用改過的 `model` / `args` / `identity` / `env`。
- DELETE Project：需所有 Bot 已停止 → TOML 移除 → 不關 workspace（第二階段）、不刪目錄。

### 6.5 對帳（daemon 啟動、事件連線重連；逐 bot 在鎖內）
1. `session.snapshot` + `agent.list`。
2. DB 中 active Run：
   - agent 清單中有 `name == bot.name` → 維持，更新 `pane_id`（pane move 會改 id）與 `agent_status`——`agent_status` 不用清單上的，
     在鎖內再 `agent.get` 一次（清單是拿鎖前讀的，前一顆 bot 的 start 可能握鎖一分鐘；用舊的 `idle` 蓋掉已經 `working` 的 run，
     真正的 `working→idle` 事件會因 `prev == idle` 不啟動備援、不送排隊 prompt；2026-09-12 review a）。
   - 否則 → Run `exited`。
3. agent 清單中 `name` 匹配某 Bot 但 DB 無 active Run → 建 Run（`running`，`adopted=1`），沿用同一 Conversation。
4. **orphan pane 回收**（第一階段）：DB 中 `exited/stopped` Run 記錄的 `pane_id` 若仍存在於 snapshot 且無 agent → `pane.close`。
5. 重建各 active Run 的狀態訂閱。

### 6.5a 子 agent 認領（血緣優先，2026-09-07）

一個 bot 一個 tab。對帳走完上面的逐 bot 迴圈後，`agent.list` 裡**沒有任何 bot 認領**的 agent 依序試兩條線索：

1. **血緣（優先）**：它的 `tab_id` 等於某個 bot 活動 run 的 `tab_id`（該 bot 的 agent 這一輪在 herdr 裡還在）
   → 那個 bot 的子 agent。子 pane 是從父 pane split 出來的，所以它必然在父的 tab 裡；這是機制，
   不需要 agent 配合。同一個 tab 裡有多個 bot（父 + 已認領的子）時，取名字前綴最長的那個，
   平手時取非 `child` 的那個 bot——孫代因此掛在子代下面。
2. **名字前綴**：名稱是 `<某 bot 的 agent 名>-<字尾>`，取最長匹配。跨 tab 與 team workspace 的情況只有這條線索。

兩條都命中時以血緣為準。認領後走同一條既有路徑：`managed_by='child'`、`parent_bot_id`、`adopted=1` 的 run、
**同一個父 bot 底下**同名的既有 live child 直接重用。子 bot 的 `name`：有前綴就取字尾，否則用 herdr 的 agent 名（去掉空白與 `@,:;`、截到 32 字）。
字尾在專案裡已經是別人的名字（另一顆母 bot 的同名子 agent、或使用者自己建的 `review` 撞上 `<parent>-review`）時，
改用完整的 herdr agent 名存（herdr 保證唯一），下次認領兩種拼法都找得到。每顆子 agent 的認領各自成敗：
一顆失敗只寫 log 跳過，不中止整台主機的對帳（2026-09-12 review #2：以前名字撞到 `?` 直接讓整輪中止、每兩秒重複）。

**子 agent 退役（#60，2026-09-11）**：子 agent 只活在它的 pane 裡，所以 pane 沒了它就退役（`bots.deleted_at`，對話保留）。兩條路都要接得住：
reconcile 發現 run 還在、agent 卻不見（原本就有）；以及 `herdr pane close` 時 `pane_closed` 事件**先**把 run 結束、reconcile 後到——
這時 bot 沒有 active run、herdr 清單也找不到它的 agent 名，且它至少有一個已結束的 run，就一樣退役。herdr 還列著這個 agent 的（例如 pane 被搬走、
舊 pane id 被報成關閉）不算，會被重新收編。`pane_closed` 結束的若是子 agent 的 run，daemon 會在 2 秒後自己排一次 reconcile，不必等下一個不相干的事件。

### 6.5b herdr PATH shim（把命名規則變成機制，2026-09-07）

daemon 每次起 pane 前，把一支 POSIX `sh` 包裝腳本裝到 `<bot 目錄>/bin/herdr`
（遠端走 `hook.sh` 同一條 ssh 路徑，`<remote bot dir>/bin/herdr`），並把那個目錄放到 pane 的 `PATH` 最前面。

- `herdr agent start <name> …`：`<name>` 不是以 `$AM_AGENT_NAME-` 開頭就自動補上前綴（截到 herdr 的 32 字上限），
  並在 stderr 印一行說明。旗標可以在名字前面，`--kind` / `--pane` / `--timeout` 的值不會被誤認成名字，`--` 之後原封不動。
  **模型沿用**（2026-09-08）：`--` 之後沒有 `--model` 且 `--kind` 與母 bot 相同（或沒寫）時，補上 `-- --model $AM_MODEL`，
  claude 再補 `--effort $AM_EFFORT`（子 agent 自己有寫的一律尊重：`--model`、codex／grok 的 `-m`、codex 的 `-c model=`
  都算；effort 方面 codex 的 `-c model_reasoning_effort=` 也算有寫）。
  不然子 agent 跑 CLI 預設，側欄多一顆「claude-fable-5-1」跟母 bot 的 `opus` 對不上。
- `herdr pane split` / `pane new` / `tab create`：原樣轉發，另外補上 `--env`
  把 `CLAUDE_CONFIG_DIR`、`CODEX_HOME`、`AM_BOT_ID`、`AM_HOOK_TOKEN`、`AM_PORT`、`AM_RUN_ID`、`AM_AGENT_NAME`、`AM_KIND`、`AM_MODEL`、`AM_EFFORT`、`PATH` 帶下去
  ——herdr 的 pane 是 **server** 生的、不繼承呼叫端 shell，沒有這一段子 pane 會用使用者的預設帳號起來、也拿不到 hook token。
  呼叫端自己給過的同名 `--env` 保留不動。
- 其他子指令 `exec` 真正的 herdr：`$AM_REAL_HERDR`，否則掃 `PATH` 取第一個不是自己所在目錄的 `herdr`。

`pane_env` 因此多 `AM_AGENT_NAME`（= run 的 agent 名）、`AM_KIND` / `AM_MODEL` / `AM_EFFORT`（母 bot 的設定，沒設就沒有）與 `PATH`。

**PATH 只靠 pane env 是不夠的**：herdr 用 **login shell** 開 pane，使用者的 profile 在那之後才跑並重建 `PATH`
（2026-09-07 實測 macOS：`/etc/zprofile` 的 `path_helper` 加 `brew shellenv` 會把 shim 擠到 `/opt/homebrew/bin` 後面）。
所以 daemon 在 `agent.start` 前再對 pane 自己的 shell `pane.send_text` 一行 ` export PATH=<dir>:"$PATH"`——
它跑在 profile 之後，才是真正生效的那一次。裝不起來（遠端 ssh 失敗等）不會擋 bot 啟動：§6.5a 的血緣認領仍然追得到。

子 agent 要指定自己的 pane 時用 herdr 自己注入的 `$HERDR_PANE_ID`（或 `--current`），不需要另外一個變數。

### 6.5d agent 對 agent 的 prompt 要標出來源（2026-09-12）

bot 之間互相派工有兩條路。走 daemon 的（`POST /api/bots/{id}/prompt`、總管的 assignment）會寫
`messages.relay_from`，UI 畫成「AGM → 這顆 bot」。另一條是 agent 自己 `herdr agent prompt <名字> …`：
daemon 沒有參與，那句話只以 **prompt 回音**的形式從 hook 回來（`source='hook'` 的 user 訊息），於是
總管的裁示在對話裡跟使用者自己打的字長得一模一樣（2026-09-12 使用者：「這種 agm 的訊息標示為 agm 訊息」）。

補法跟 §6.5b 同一種：**做成機制，不是請求**。

1. PATH 上的 shim 攔 `agent prompt`：目標名字 herdr 本來就認得（`herdr agent get` 找得到——AGM、別的頂層 bot、
   pane id）就照原名送，找不到才視為自己的子 agent 補前綴（不然向 AGM 申請會被改成 `<自己>-agm-…` 而 unknown_target）；
   決定好名字之後，先 `POST /relay/announce`
   （表單欄位 `bot_id`／`to_agent`／`text`，驗證用該 bot 的 `hook_token`，header `X-AM-Bot-Token`，
   跟 hook 同一把鑰匙），再照常轉給真的 herdr。報不成功（沒有 curl、daemon 沒開）就只是少一次標示，
   訊息照送。名字前面帶旗標時整串原樣轉發，不猜。
2. daemon 把「誰要送什麼給哪個 agent」記在一張行程內的短命表（5 分鐘）。
3. 那句話的回音從 hook 回來時，用 run 的 `agent_name` 去認領：比對時空白全部忽略（TUI 會在任意位置
   折行、補縮排），長度取兩邊的較短者，至少要對上 12 個字元；短於此就要求完全一樣（「繼續」這種字
   使用者自己也會打）。認到就在**插入當下**寫進 `relay_from`——事後補欄位的話，`message_added` 已經
   推出去了，畫面上那顆泡泡要等重新載入才會變。
4. 認不出來就維持 NULL＝使用者自己打的。寧可少標一次，也不要把使用者的話說成是別人送的。

### 6.5c 給 claude 注入 herdr skill（2026-09-07）

啟動 claude bot 前，daemon 把 `herdr --skill` 的輸出寫到那個身份的
`$CLAUDE_CONFIG_DIR/skills/herdr/SKILL.md`（沒設就是 `~/.claude/skills/…`；遠端用 ssh 跑 `herdr --skill` 再寫回去）。
內容相同就不寫——那是使用者自己的 claude 設定，每次啟動都改一次 mtime 只是雜訊。

寫進去之前改兩個地方：

1. frontmatter 的 `description` 換成 AG Man 的版本。herdr 原文寫「只有使用者明確提到 Herdr 才用，
   不要只因為工作可能受益於背景終端或平行處理就用」，對住在 AG Man 裡的 bot 剛好相反：開子 agent 就是重點。
2. body 最前面插一段 **AG Man 規則**（`lifecycle::child_agent_rules`）：開新的之前先 `herdr agent list`
   找自己底下閒置的 child 來重用、子 agent 命名、
   `herdr pane split --pane "$HERDR_PANE_ID"`（或 `--current`）、不要 `git stash` / `--autostash`、
   子 agent 會被掛在自己底下追蹤、帳號與 hook 會自動帶進子 pane。
   另有一段**瀏覽器的用法**（2026-09-08）：一律用 ego lite（`ego-browser` skill）、一個 bot 最多一個分頁
   （母與子各算一個，task space 用自己的 agent 名）、bot 結束就 `closeTab` / `completeTaskSpace({ keep: false })`。

herdr 自己寫的 CLI 說明原樣保留，所以 herdr 升級會把新文字一起帶進來。裝不起來只留 warning，claude 沒有 skill 照常跑。

`child_agent_rules` 是**同一份文字來源**：claude 的 skill、以及三種 kind 的 persona
（`--append-system-prompt` / `--rules` / `developer_instructions`）都用它，所以 codex / grok 拿到的是同一段規則。

### 6.5.1 採用使用者的 Herdr `default` session

daemon 另以唯讀優先的方式觀察本機 Herdr `default` session（socket 為
`~/.config/herdr/herdr.sock`），不替它啟動 server。每次啟動、事件重連及定期輪詢時：

1. 取得 `agent.list`；只處理支援的 `claude` / `codex` / `grok` agent。
2. 以 agent 的 `foreground_cwd`（沒有時用 `cwd`）與既有 local Project 的 canonical path
   **完全相等**來配對；不自動建立 Project，也不採用其他工作目錄或普通 shell pane。
3. 找到既有採用紀錄就更新其 Run；否則建立一個 `herdr_session = "default"` 的 Bot 設定並
   建立 `adopted = 1` 的 active Run。Bot 設定寫回 `config.toml`，因此 daemon 重啟後仍保留。
4. default session 的 workspace 不寫入 `projects.workspace_id`；default pane 消失只會結束 Run，
   不會由 daemon 回收或關閉該使用者 pane。同一條線的另外三處（2026-09-12 review #4）：`stop` 只送 ctrl+c、
   **不 `pane.close`**；`start` / `restart` 對 `herdr_session = "default"` 的 bot 回 409 `default_session`
   （restart 在送 ctrl+c 之前就拒絕）；§6.9 的批次重啟把它列為 `default_session` 跳過。

default Bot 的 prompt / keys / terminal 讀取會依 Run 的 session 回到 default socket；沒有 hook
注入的既有 agent 仍透過 pane status 與 terminal fallback 更新對話。

### 6.6 事件處理
- `pane.agent_status_changed`：更新 Run `agent_status`；`working→idle` 啟動備援計時（§4.3）；推 WS。遠端 run 另外先 drain 一次該 bot 的 spool（§11.4.3）。
- `pane.exited` / `pane.closed`：對應 Run → `exited`，in-flight Turn → `failed`。
- `workspace.closed`：`projects.workspace_id = NULL`，其下 Run → `exited`。
- `pane.agent_detected`：僅 log。

### 6.7 hook 與 Turn 的配對（在 per-bot 鎖內）
1. 驗 token；解析 active Run；無 → `origin=external` 處理（下方第 5 點）。
2. 事件分類：
   - Claude `SessionStart` / grok `session_start` / Codex 首次任何事件 → 回填 `runs.native_session_id`、`transcript_path`，**不建 Turn**。
   - Claude `Stop`（`stop_hook_active=false`）/ Codex `agent-turn-complete` / grok `stop`（`reason = end_turn` 且 `stopHookActive = false`）→ 進入配對。
   - 其他 type → ack 後丟棄。
3. 去重：`(native_session_id, native_turn_id)` 已存在 → 忽略。
4. 配對目標 = 該 Run **唯一**的 `in_flight` Turn（不用時間排序）：
   - 有 → CAS `UPDATE … WHERE status='in_flight'`；**成功**才建 assistant Message（`source=hook`），Turn `completed`，寫入 native ids。
     CAS 輸了且 Turn 已是 `completed_fallback`（§4.3 的備援先收掉）→ 只把 native ids 蓋到那筆 Turn、丟棄 payload
     （2026-09-12 review #5：以前不看 CAS 結果照樣插一則，同回合兩則 assistant）。其他原因收掉的（stop／failed）照舊保留回覆。
   - 無 → 第 5 點。
5. external：建 Turn（`origin=external`, `status=completed`）+ user Message（Codex 可從 `input-messages` 取得；Claude 無則省略）+ assistant Message。
6. 推 WS `message_added` / `turn_updated`。

### 6.9 一鍵套用 claude 更新（批次 exit + resume，v4.4）

> **2026-09-11 競態修正**（事故：2026-09-10 23:02，AGM 與另外三顆 bot 的 pane 被關、5.5 小時沒人拉回）。
> 原本 stop 與 start 各拿一次 bot 鎖，中間有空檔；同時在跑的 reconcile 在拿鎖**之前**就讀了 agent 清單，
> 排在鎖上等，stop 一放鎖就搶先進來，看到「沒有 active run、清單上卻還有這個 agent」，把剛被停掉的 agent
> 收編成新 run；start 接著以 `active run already exists` 放棄，pane-closed 事件再把收編的 run 結束——
> bot 從此沒人啟動。修正四件：
> 1. `restart_bot_with`（批次與 `/bots/{id}/restart` 共用）在**一次持鎖**裡做完 stop + start，不留空檔。
>    若 start 仍被一個 pane 已不存在的 run 擋住，就把那個 run 結束並再試一次。
> 2. reconcile 拿到鎖之後，若要**收編**（沒有 active run）或要**改寫 run 的 pane**（清單上的 pane 與 run 記錄不同），
>    先向 herdr 重新 `agent.get` + `pane.get` 確認；agent 已不在或 pane 已關就不收編、不改寫。RPC 失敗時維持原判斷。
>    有 active run 但 herdr **按名字**找不到 agent 時，不能只憑這點把 run 標 exited：herdr 0.8.2 的名字是一份
>    以名字為鍵的登錄，同名 agent 在新 pane 重開後，舊 agent 晚到的退出處理會把新 agent 剛拿到的名字清掉
>    （`agent.list` 裡它變成 `name: null`、`agent.get <name>` 答 not-found，但 `pane.get` 仍回報 `agent: claude`）。
>    2026-09-11 第一版修正就是這樣把整批剛重啟的 run 標成 exited、再由孤兒 pane 清掃把新 pane 關掉。
>    所以改成問 run 自己的 pane：`agent.get <pane_id>`——pane 裡是它自己的 agent（名字相同或已被清掉）就保留 run、
>    用 `agent.rename <pane_id> <name>` 把名字補回去（之後 `agent.prompt`／`send_keys` 才叫得到）；pane 裡是別人的
>    agent 或空的才標 exited；RPC 失敗就這一輪不動。`run_alive` 同理以 pane 本身有沒有 agent 為準，`agent.get` 只是備援。
> 3. 批次裡某顆最後仍啟動失敗：若它留下一個 pane 已關的 run 就結束掉（bot 顯示為停止，可再啟動），並推一則
>    supervisor inbox 事件 `kind = bot_restart_failed`（payload：`batch_id`、`bot_id`、`name`、`error`）。
> 4. 總管 bot（`supervisors.bot_id`，即 AGM）**排在最後**重啟；重啟後 60 秒內每 5 秒檢查一次它是否 running、
>    pane 還在、agent 還在，沒回來就自動再啟動一次，結果推 `kind = supervisor_restart_retry`（payload 含 `ok`、`error`）。

claude 把新版下載好之後只會在每顆 bot 的 pane 底下印 `Update installed · Restart to update`
（daemon 收在 `runs.update_notice`，見 §4.5 / API.md），套用的唯一方式就是重啟。十顆 bot 就是點
十次「重啟」，而且每點一次都要先自己確認那顆有沒有在忙。

- 入口：`POST /api/bots/restart-idle`（無 body）。**立刻回計畫就結束**，實際重啟在背景跑——一顆
  `stop_bot` 最久要等 agent 十秒才放棄，五顆就一分鐘，同步做完再回會把 HTTP 連線拖死。
- 挑選（`daemon/src/bulk_restart.rs::plan`，純函式、有單元測試）。候選 = **kind 是 claude**
  且該 run **帶著非空的 `update_notice`**；不是候選的（其他 kind、沒有更新在等的）連「跳過」都不
  列，那不是使用者按這顆按鈕時在問的事。候選裡依序判斷，第一個中的就是回報的理由：

  | 條件 | `reason` | 動作 |
  |---|---|---|
  | `bots.managed_by = 'team'` | `team_member` | 跳過 |
  | run 或 bot 的 `herdr_session = 'default'`（§6.5.1） | `default_session` | 跳過 |
  | `runs.state != 'running'` | `not_running` | 跳過 |
  | `agent_status = 'working'` | `working` | 跳過 |
  | `agent_status = 'blocked'` | `blocked` | 跳過 |
  | `agent_status` 不是 `idle`（`unknown`） | `unknown_status` | 跳過 |
  | 該 run 還有 `in_flight` Turn | `turn_in_flight` | 跳過 |
  | 以上都不中 | — | 重啟 |

  批次操作最不能做的事就是把使用者正在等的那一回合砍掉，所以規則刻意保守：`unknown` 也跳過。
  `team` 不歸這顆按鈕管：成員的 run 由 team 排程記著，插手會讓排程對不上。
  **子 agent 進來**（2026-09-12 使用者：ns2 / race / sup 三顆全被跳過，子 agent 的更新永遠套不上去）。
  它們不能照一般路徑重開 pane（§6.5a，`start_bot` 會拒絕），所以執行時改走
  `lifecycle::restart_child_in_pane`：送 `ctrl+c` 讓 agent 退出、**不關 pane**，再用同一個 agent 名字在
  同一個 pane 上 `agent.start`，帶 `--resume <上一個 session>`、`bots` 上那份模型／強度（§4.4a 從它自己的
  argv 讀回來的）與 `auto_approve` 對應的旗標。pane 的環境（`CLAUDE_CONFIG_DIR`、PATH 上的 shim）留在
  pane 的 shell 裡，所以帳號與工具不變；hook 一樣沒注入，回覆照舊走 §4.3 的終端快照。收 agent 的過程中
  pane 不見了（父 agent 自己關掉）就把 run 標 exited、不重開。agent 十秒內**沒退出**（卡在 modal、對 ctrl+c
  沒反應）就回 502、不動它的 pane，而且 run 要從 `stopping` **放回 `running`**——agent 還在 pane 裡，run 就還活著
  （2026-09-12 review #1：以前留在 `stopping`，之後 prompt 一律 409、側欄永遠黃燈）。§6.5 的對帳同樣把 herdr
  仍列著 agent 的 `stopping` run 轉回 `running`。單顆的 `POST /api/bots/{id}/restart` 對子
  agent 走同一條路。
- 執行：一顆一顆、**序列**跑，每顆都是 `lifecycle::restart_bot_with(StartOpts { resume_native: true })`
  ——stop 與 start 在**同一次持有 bot 鎖**裡做完（見下方「2026-09-11 競態修正」）。也就是既有的單顆路徑加上 §6.2 的續接旗標——`stop_bot` 寫上 `ended_at`
  之後，剛結束那個 `native_session_id` 就成了 `last_native_session_id` 找得到的「上一個 session」，
  claude 拿到的是 `--resume <session>`。沒有另一套啟動流程，hook 注入 / 身份 / 模型 / pane 版面全部照舊。
  - **沒寫過 transcript 的 session 不續接**：claude 只在對話有訊息後才建 transcript，對一個從沒被
    prompt 過的 bot `--resume <id>` 會印 `No conversation found` 立刻退出——herdr 看到 TUI 一瞬間、
    批次回報成功，一秒後 pane 變回 shell、reconcile 把 run 標 exited、孤兒清掃關 pane（2026-09-11 隔離
    重現裡每一輪都是這樣 0/4）。所以本機 host 上 hook 回報過的 `transcript_path` 不存在時，改開新對話
    （`member_context_lost("transcript_missing")`），不帶 `--resume`。
  - 與 `POST /bots/:id/restart` 的差別只有這個旗標：那條是「重新開始」，這條是「接著跑」。
- **一顆失敗不中斷整批**：批次的價值就在於不用一顆一顆顧，中途停下等於白做。失敗的記在結果裡。
- 序列而不是並行：herdr 的 pane 版面（§6.2 的挑最大面積切）與每顆的 per-bot 鎖都假設一次一顆，
  並行重啟五顆會互相搶版面，而且錯誤訊息會混在一起分不出是誰的。
- 回饋走 WS：`bots_restart_progress`（每顆兩次：`restarting` / `ok` 或 `failed`）與
  `bots_restart_done`（最終的 `ok` / `failed` / `skipped` 三張清單）。前端用它畫「第幾顆 / 共幾顆」
  與最後的摘要，見 `docs/UI-DECISIONS.md`。
- 入口在**額度列**上（`web/src/components/UpdateQuotaChip.tsx`）：`⬆ N` 一顆 chip，**貼在 claude
  那幾格量表的右邊**——「claude 有沒有新版」跟「claude 還剩多少額度」都是這個 kind 的全域狀態，而額度列
  在每個畫面的標題列上都在。按下去先跳確認框列出要重啟哪幾顆、會跳過哪幾顆，確認後才打這支 API。
  側欄那條（`UpdateAllBanner`）只留按下去之後的進度與失敗／跳過名單。


## 7. API

### 7.1 存取控制
- bind：開發版一律 bind `0.0.0.0`（每張網卡），只有打包成 macOS app 的執行檔（路徑在
  `…app/Contents/MacOS/`）才 bind `127.0.0.1`；`AM_DEV_LAN` 可雙向覆寫。見 README 與
  `daemon/src/main.rs::dev_lan_default`。
- daemon 啟動時產生 UI token 寫入 `~/.config/agents-manager/ui-token`；`GET /api/session`（檢查 `Host` 為 `127.0.0.1:<port>` 或 `localhost:<port>`）回傳 token；其餘 `/api/*` 需 header `X-AM-Token`；`/ws` 以 `?token=`。同時檢查 `Origin`（若存在）為本機。
- `/hook/*` 驗 **per-bot** `X-AM-Bot-Token`。

### 7.2 REST（前綴 `/api`）
| 方法 | 路徑 | 說明 |
|---|---|---|
| GET | `/session` | `{token}` |
| GET | `/state` | projects、bots、active runs、agent 狀態、`daemon_seq` |
| POST | `/projects` | `{path,label}`；路徑正規化；重複 409 |
| DELETE | `/projects/:id` | §6.4 |
| POST | `/projects/:id/bots` | `{name,kind,args,autostart}` |
| PATCH | `/bots/:id` | `{name?, model?, args?, autostart?, auto_approve?, inject_hooks?, identity?, env?}` → `{needs_restart}`；只有 `name` 變更需無 active Run（v3.3） |
| DELETE | `/bots/:id` | §6.4 |
| POST | `/bots/:id/start` | 200 `{run_id}`；已有 active Run → 409 `{run_id}` |
| POST | `/bots/:id/restart` | 200 `{run_id}`；等同 stop（若有 Run）+ start（v3.3） |
| POST | `/bots/:id/stop` | 200；無 Run → 204 |
| POST | `/bots/:id/interrupt` | 送 esc |
| POST | `/bots/:id/prompt` | `{text, client_request_id}` → 200 `{turn_id, message_id, delivery}`；409 見 §6.3 |
| POST | `/bots/:id/keys` | `{keys:[...], expect_run_id}`；run 不符 409 |
| POST | `/bots/:id/text` | `{text, enter?, expect_run_id}`——整段文字打進 pane（多行原樣），預設接 Enter |
| POST | `/turns/:id/abandon` | in-flight / delivery=unknown → `failed` |
| GET | `/bots/:id/messages?before=&limit=` | 倒序分頁 |
| GET | `/projects/:id/messages?before=&limit=` | 群組時間軸：該 Project 所有 bot 的訊息合併，每則帶 `bot_id` / `bot_name`（§13） |
| POST | `/projects/:id/chat` | `{text, client_request_id}` → `{group_id, sent, skipped}`；`@<bot>` / `@all` fan-out（§13） |
| GET | `/bots/:id/terminal?source=visible|recent_unwrapped&lines=` | 快照（含 revision / truncated） |

### 7.3 WebSocket `/ws`
- 事件帶遞增 `seq`（記憶體，daemon 重啟從 0）。客戶端帶 `?since=`；daemon 保留最近 200 則，無法補齊或 seq 倒退 → 送 `{"type":"resync"}`，客戶端重新 `GET /state` 與各 bot 訊息。
- 事件：`bot_status`、`message_added`、`turn_updated`、`project_changed`、`bot_changed`、`daemon_status`。`terminal_snapshot` 推送為第二階段（第一階段前端輪詢 `GET terminal`）。

## 8. 技術選型
- Rust：axum 0.8、tokio、serde/serde_json、sqlx 0.8（sqlite）、toml（serde）、ulid、clap、tracing、reqwest（hook 子命令用）、rust-embed（M8）。
- 前端：Vite、React 19、TypeScript、Zustand、自寫 CSS（深淺色）。
- 專案結構：`daemon/`（單一 crate，bin `agents-managerd`，子命令 `serve` / `hook`）、`web/`。

## 9. 非目標（第一階段）
遠端存取、遠端 herdr、xterm.js 串流、bot 互相對話（使用者對多個 bot 的群組發言見 §13）、diff 檢視、transcript 回補、Codex notify chain、`toml_edit` 保註解、WS terminal 推送、未讀計數（§13 群組視圖的前端記憶體計數除外）、Project 刪除時關 workspace。


## 11. 遠端主機（Remote hosts，v3.1）

### 11.1 目標
Project 可以位於另一台機器：該機器上有自己的 herdr，agent 在那台機器的 pane 內執行，daemon 仍在本機，UI 操作方式完全相同。herdr 本身的 `--remote` 只支援 TUI attach，因此本系統以 **OpenSSH 轉發**達成（附錄 E 已實測）：

```
本機 daemon ──(ssh -M master)──► 遠端 sshd
   │  -L <本機短路徑>.sock : ~/.config/herdr/sessions/<session>/herdr.sock   （herdr RPC / 事件）
   └─ ssh <host> '<sh 指令>'                                                 （放 hook 腳本、settings、讀 spool、列目錄）
```

**沒有反向轉發**（v4.3）：遠端 hook 不再打 HTTP 回本機，狀態走 herdr 事件、內容走 spool 檔（§11.4）。
`-R` 與 `hook_port` 一起拿掉的理由見 §11.4 開頭。

### 11.2 設定
```toml
[[hosts]]
name = "m4p"                       # 唯一識別，[a-z][a-z0-9_-]{0,31}
ssh = "m4p@100.112.229.82"         # ssh 目標；可含 ssh_config 別名；port 以 ssh_port 指定
ssh_port = 22
herdr_session = "agents-manager"   # 遠端 named session（絕不使用遠端 default session）
remote_path = "/opt/homebrew/bin:$HOME/.local/bin"   # 非互動 ssh shell 缺少的 PATH，前置到 PATH
# hook_port = 7788                 # v4.3 起未使用（見 §11.4）；還在檔案裡的話會被忽略並 warn 一次

[[projects]]
host = "m4p"                       # 缺省 = 本機
path = "/Users/m4p/work/foo"
label = "foo@m4p"
```
- 認證只用使用者現有的 ssh key / agent / ssh_config；一律 `BatchMode=yes`，**絕不**互動輸入密碼。認證失敗 → host `disconnected` 並在 UI 顯示錯誤字串。
- `hosts[].name` 為 `"local"` 保留給本機，不可設定。

### 11.3 HostManager（daemon）
每個 host 一個 `HostConn`：
1. **ensure remote session**（v3.4）：遠端為 macOS 且 ssh 使用者就是 `/dev/console` 的擁有者時，寫入 `~/Library/LaunchAgents/dev.agents-manager.herdr-<session>.plist`（`ProgramArguments = herdr --session <session> server`、`KeepAlive`、`RunAtLoad`、`ProcessType Interactive`、PATH 含 `remote_path`）並 `launchctl bootstrap gui/<uid>`；若已載入則沿用；若原本有 nohup 起的 server 先 `herdr --session <session> server stop` 再交給 launchd。非 macOS、無桌面登入或 launchctl 失敗 → 退回 `( trap '' HUP; herdr --session <session> server & )`。
   - 原因：非互動 ssh 工作階段讀不到使用者的登入 Keychain（`security` 回 errSecInteractionNotAllowed，錯誤 36），在該 herdr 底下啟動的 Claude Code 會顯示「Not logged in」即使主機已登入；GUI 網域的 LaunchAgent 跑在桌面工作階段，Keychain 已解鎖。附帶好處：ssh 斷線或 herdr 當掉 launchd 會自動拉起。
2. **master 連線**：`ssh -N -M -S <ctl> -o BatchMode=yes -o ExitOnForwardFailure=yes -o ServerAliveInterval=15 -o ServerAliveCountMax=3 -o StreamLocalBindUnlink=yes -L <local.sock>:<remote herdr.sock> <target>`。
   - `<local.sock>` 與 `<ctl>` 必須放在**短路徑**（macOS AF_UNIX 上限 104 bytes）：`/tmp/agents-manager-<uid>/<host>.sock`、`<host>.ctl`。
   - 只剩 `-L` 一條轉發。`ExitOnForwardFailure=yes` 仍然保留：沒有 herdr socket 的 master 沒有用；但這條是**本機**綁定、由 `StreamLocalBindUnlink=yes` 自己收拾，不會像舊的 `-R` 那樣被遠端的殘留佔用卡死。
   - v4.2 的「反向通道埠探測」表（`AM_FWD=free|live|busy`、殺掉死掉的 sshd 收回埠）連同 `-R` 一起移除：遠端已經不需要任何 daemon 的埠。歷史原因見 §11.4。
3. 以 `HerdrClient::new(<local.sock>)` 取得與本機完全相同的 client；`ping` 成功 → `connected`。
4. 健康檢查：每 10 秒 `ping`；失敗或 master 程序退出 → 標 `disconnected`、指數退避（1s→30s）重建 master → 成功後對該 host 執行對帳（§6.5）並重建事件訂閱。
5. daemon 退出時關閉 master（`ssh -O exit`）；遠端 herdr server 與 agent 保持存活。
6. `App` 由單一 `herdr` 改為 `hosts: HashMap<String, HostConn>`，`"local"` 為既有本機 client；所有用到 `app.herdr` 的地方改為 `app.herdr_for(project.host)`。pane watcher、fallback timer 等以 `(host, pane_id)` 為鍵。對帳與全域事件訂閱逐 host 執行。

### 11.4 遠端 hook：herdr 事件 + spool 檔（v4.3）

遠端沒有 `agents-managerd` 二進位，hook 仍是 daemon 寫過去的 **POSIX sh** 腳本；改掉的是它「怎麼把事情講回來」。

**為什麼不再用反向埠**：v3.1–v4.2 的做法是 `ssh -R <hook_port>:127.0.0.1:<daemon port>`，遠端 hook 用 curl 打
`http://127.0.0.1:<hook_port>/hook/<provider>`。這條路有兩個結構性問題：
1. `ExitOnForwardFailure=yes` 讓「遠端那個埠被佔」等於**整台主機標為未連線**——連 herdr 都連不上，UI 只看得到
   `ssh master exited immediately`。實測有過一條沒清乾淨的舊 ssh 通道把埠佔了 51 分鐘。
2. `hook_port` 預設等於 daemon 埠 7788，遠端只要自己也跑一份 agents-manager 就必撞。

而 daemon 早就有一條到遠端的可靠通道：`-L` 轉發的 herdr socket。所以改成**混合路徑**：

| 走什麼 | 用什麼 | 為什麼 |
|---|---|---|
| 狀態（working / idle / blocked、native session id） | 遠端 hook 呼叫該機器上的 `herdr pane report-agent` | 事件經 herdr socket 自然回到 daemon，不需要任何額外通道 |
| 內容（完整事件 JSON：`last_assistant_message`、`prompt_id`、`transcript_path`…） | 追加到遠端 `~/.config/agents-manager/bots/<bot_id>/hook-spool.jsonl` | herdr 事件**不帶** hook 的 payload（見下），內容只能落地再由 daemon 讀 |

本機 bot 完全不受影響：仍走 `agents-managerd hook <provider>` → HTTP `POST /hook/<provider>`（§4.4）。

#### 11.4.1 herdr 端的事實（0.8.2 實測，2026-09-07，附錄 E 補記）
- `herdr pane report-agent <PANE_ID> --source <ID> --agent <LABEL> --state <idle|working|blocked|unknown>
  [--message <TEXT>] [--seq <N>] [--agent-session-id <ID>] [--agent-session-path <PATH>]`。
- 狀態**真的**會變成訂閱端收得到的 `pane.agent_status_changed`，事件 data 為
  `{pane_id, workspace_id, agent, agent_status}`——**沒有** `message`、`source`、`seq`、`agent_session_id`。
  所以 `--message` 不能拿來送內容，只能當人看的旁註；內容一律走 spool。
- `--seq` 是**每個 `(pane, source)` 各自**的單調計數：小於等於上一次的會被丟掉（實測 `seq=11` 之後送 `seq=5` 不生效，
  換一個 `--source` 則從 1 開始就生效）。所以每個 bot 用自己固定的 `source`，`seq` 必須嚴格遞增。
- `herdr pane report-agent-session …` 在 0.8.2 **不發任何事件**，也沒出現在 `api snapshot` / `agent list`。
  因此它只當「告訴 herdr 這個 pane 的 native session」的 best-effort，**daemon 不靠它拿 session id**——
  session id 從 spool 的 payload 讀（§6.7 原本就這樣做）。
- 狀態上報與 herdr 自己的終端偵測**並存**：同一個 pane 兩邊都會報。相同的狀態不會再發一次事件，
  所以「終端偵測先報了 idle、hook 隨後才寫 spool」是真實存在的競態，daemon 端要用 §11.4.4 的重試補。

#### 11.4.2 遠端 `hook.sh` 的行為
路徑不變：`~/.config/agents-manager/bots/<bot_id>/hook.sh`，由 daemon 在每次啟動 Run（與 reconcile 修好 hook 時）覆寫。

argv 維持 `hook.sh <provider> <bot_id> <token> [port]`——**第 4 個參數保留但忽略**，因為 codex 的 `-c notify=[…]`
與 grok 分派腳本的 argv 是啟動當下寫死的，升級 daemon 時遠端還活著的 agent 仍會用舊 argv 呼叫。第 3 個參數的位置
同理保留，但 daemon 從 2026-09-12 起**填 `-`**、不再把 `hook_token` 放上去（review #8）：`hook.sh` 從來不讀它、spool 行
也不含它、daemon 重放 spool 時也不驗它，而 codex 的 notify argv 掛在整個 run 的程序上，`ps` 對該主機所有使用者可見——
那把 token 同時是本機 `/hook/*` 與 `/relay/announce` 的鑰匙。舊 agent 仍帶真 token 呼叫也照收。不再有 HTTP 可打，因此不再需要 curl。

每個 provider 的行為：

| provider | payload 來源 | 上報狀態 | 寫 spool |
|---|---|---|---|
| `claude` | stdin（≤1 MiB，超過截斷並標 `truncated`） | `SessionStart` → 不報狀態（只 `report-agent-session`）；`Stop` 且 `stop_hook_active=false` → `--state idle` | 兩者都寫 |
| `codex` | argv 最後一個參數（JSON） | `agent-turn-complete` → `--state idle` | 寫 |
| `grok` | stdin | `session_start` → 只 `report-agent-session`；`stop` 且 `reason=end_turn` 且 `stopHookActive=false` → `--state idle`；`reason=shutdown` → 不報 | 寫（分類仍由 daemon 做，見下） |
| `statusline` | stdin | 不報狀態 | **不進 spool**，見 §11.4.5 |

- **腳本不做語意判斷**：`hook.sh` 只用最粗的字串比對決定「這是不是回合結束」（`"stop_hook_active":true` / `"reason":"shutdown"`
  出現就不報 idle），其餘一律照寫 spool，真正的分類仍然只在 daemon 的 `hookrecv::classify`（§6.7 / §12.3）。
  理由：遠端腳本沒有測試，錯了很難查；漏報一次狀態最多晚一點被掃到，錯誤分類會直接吃掉訊息。
- 寫檔順序：**先寫 spool，再 `report-agent`**。反過來會讓 daemon 收到事件時 spool 還沒有那一行。
- spool 行格式與 §6.7 / §4.4.4 完全相同（`{bot_id, provider, payload, received_at, truncated}`），一行一筆，`O_APPEND`。
- `report-agent` 的欄位怎麼填：

  | 欄位 | 值 |
  |---|---|
  | `<PANE_ID>`（位置參數） | `$HERDR_PANE_ID`（herdr 注入 pane env；沒有就跳過整個上報） |
  | `--source` | `agents-manager:<bot_id>`——固定、每個 bot 一個，`--seq` 的單調性以此為界 |
  | `--agent` | bot 的 kind（`claude` / `codex` / `grok`），與 herdr 的 agent label 一致 |
  | `--state` | 只送 `idle`（回合結束）。`working` 由 herdr 的終端偵測負責——hook 沒有「開始工作」的事件，硬報會跟偵測互相蓋 |
  | `--seq` | 嚴格遞增：有 `python3` 用 `time.time_ns()`（與 herdr 官方 integration 同法），否則 `date +%s` 乘 1000 再加 `$DIR/hook-seq` 的計數（mod 1000） |
  | `--agent-session-id` | payload 裡的 native session id（claude `session_id` / grok `sessionId` / codex `thread-id`），缺就不帶 |
  | `--agent-session-path` | `transcript_path` / `transcriptPath`，缺就不帶 |
  | `--message` | 不填。事件不帶它，填了只是浪費 |

- 找 herdr：`${AM_REAL_HERDR:-}`（daemon 已在 pane env 給過真 binary 路徑，§6.5b）→ `command -v herdr`。都找不到就**只寫 spool**、
  在 `hook.log` 記一行、`exit 0`：daemon 仍會在終端偵測的 `working → idle` 上把它掃回來，只是慢一點。
- session 選擇：`herdr` 在 pane 內靠 `HERDR_SOCKET_PATH` / `HERDR_SESSION` 自己找對 session；腳本在 `HERDR_SESSION`
  有值時明確帶 `--session "$HERDR_SESSION"`。
- 契約仍是 §4.4：wall-clock ≤3 秒、永遠 `exit 0`、永遠空 stdout（grok 的 Stop hook 會把 stdout 當 decision）。
  `report-agent` 加 `timeout`／背景化不必要——它是本機 unix socket，實測 <20 ms。

#### 11.4.3 daemon 端：收到狀態事件 → 讀 spool → 重放
`events::handle_status`（§6.6）在**遠端** run 上多一步。時序（全部在 per-bot 鎖內，鎖與 HTTP hook 共用同一把）：

1. 收到該 pane 的 `pane.agent_status_changed`；照舊更新 `runs.agent_status`、推 WS。
2. 若 host ≠ `local` 且（`working → idle` 或 `→ blocked`）→ 觸發 **drain**（`hookrecv::replay_spool`，既有的
   `replay_spool_remote` 路徑）：
   `ssh <host>` 一段 sh：`hook-spool.jsonl` → `mv` 成 `hook-spool.jsonl.replaying`（已存在 `.replaying` 表示上次中途死掉，
   把新的接在它後面）→ `cat` 出來 → `rm`。daemon 逐行 `serde_json::from_str::<HookBody>` 後走 §6.7 的配對，最後刪檔。
3. drain 是 **await 的**（預算 4 秒），成功後才 `arm_fallback`（§4.3）——這樣終端快照備援只有在 hook 真的沒來時才會贏；
   drain 失敗或逾時就照舊 arm，`completed_fallback` 的 CAS 保證不會兩邊都寫。
4. 冪等：`.replaying` 的 rename 是遠端的原子操作，取到的行已離開 spool；重放本身再靠 §6.7 的
   `(native_session_id, native_turn_id)` 去重，所以「同一輪 herdr 報兩次 idle」「daemon 重啟後又掃一次」都只會寫一則訊息。
5. per-bot 鎖：drain 全程持鎖，同一個 bot 不會有兩條 drain 同時 `mv`；不同 bot 之間互不阻塞。

#### 11.4.4 遲到、重複與遺失
- **重複事件**：herdr 同一輪可能報兩次 `working → idle`。第二次 drain 只會拿到空檔案，成本是一次 ssh。
  因此同一個 bot 的 drain 有 **1 秒的合併窗**：窗內的第二次觸發只把「還要再跑一次」記下來，不另開 ssh。
- **事件先到、spool 後寫**（腳本被 kill、檔案系統慢）：第一次 drain 拿不到 → 在 **T+2 秒**再 drain 一次（仍早於 5 秒的
  終端備援），還是沒有就讓備援接手。
- **狀態事件整個遺失**（herdr 重啟、訂閱斷線、終端偵測先報了 idle 使 hook 的上報成為 no-op）：
  - 每台已連線 host **每 30 秒**掃一次「有 in-flight Turn 或 spool 檔存在」的 bot，做一次 drain（一台一次 ssh，
    腳本內迴圈所有 bot 目錄，不是每個 bot 一次 ssh）。
  - host 重連、daemon 啟動對帳（§6.5）照舊對每個 bot drain 一次（既有 `replay_host`）。
- **遲到的 hook**：drain 出來的行對應的 Turn 已經被終端備援收成 `completed_fallback` → 依 §4.3 的既有規則
  **丟棄不覆蓋**，只 log。
- **bot 已刪除**：`process_locked` 已擋（`deleted_at`）；遠端 spool 檔在 bot 刪除時一併 `rm -rf` bot 目錄。
- **host 斷線期間**：hook 照樣寫 spool（本機檔案，不需要 daemon 在），重連後由 `replay_host` 全部補進來。這正是
  spool 原本的用途，只是現在它從「失敗才走」變成「一律走」。

#### 11.4.5 statusLine（額度）怎麼走
claude 的 statusLine 每次重繪都會被呼叫（idle 時也會），量大且沒有回合語意，**不進 spool**（會把 spool 撐爆，
而且它只在下一次回合結束才被讀到，資訊已經過期）。改為**單槽檔**：

- `hook.sh statusline <bot> <token> [port]`：把 stdin 的 JSON 加上 `"hook_event_name":"StatusLine"` 後
  **覆寫**（不是追加）遠端 `~/.config/agents-manager/bots/<bot_id>/hook-status.json`，然後照舊 exec 使用者自己的
  statusLine 命令（讀遠端 `~/.claude/settings.json`，v4.0 邏輯不變）。不呼叫 `report-agent`。
- daemon 在**每次 drain 的同一段 ssh**裡順便 `cat` 這個檔（存在才讀，讀完 `rm`），內容當成一則
  `provider=claude`、`payload.hook_event_name=StatusLine` 的 `HookBody` 丟給 `hookrecv::process_locked`，
  走既有的 `HookKind::StatusLine` 分支（寫 `runs.status_line` / `status_json`，§14）。
- 也就是說遠端額度的更新頻率 = drain 的頻率（回合結束時，或最多 30 秒一次的掃描），而不是每次重繪。UI 上的差別
  只有「額度數字最多晚 30 秒」，可以接受。

#### 11.4.6 `hook_port` 的相容處理
- `config.toml` 的 `hosts[].hook_port` **繼續被解析**（舊設定檔不能因此開不起來），但只做一件事：
  daemon 啟動或該 host 重新設定時 `warn` 一次
  `host <name>: hook_port is ignored since v4.3 (remote hooks report through herdr; see SPEC §11.4)`。
- `POST /api/hosts` 仍接受 `hook_port` 欄位（忽略）；`GET /api/state` 的 host 物件**不再**回傳它。
  UI 的主機表單移除該輸入框（`docs/API.md` §主機 同步）。
- `HostConn::hook_port()`、`probe_hook_forward()`、`HookForward` 一併刪除；pane env 不再帶 `AM_PORT`（遠端）。

#### 11.4.7 遷移
- `hook.sh` / `claude-settings.json` / grok 分派腳本本來就在**每次啟動 Run 時覆寫**，所以新版 daemon 一啟動 bot
  就是新腳本。
- 還活著的舊 run：daemon 啟動與 host 重連的對帳（§6.5）對每個被保留的 run 重寫一次遠端 hook 素材
  （`install_remote_hook` 抽成可獨立呼叫的 `refresh_remote_hook`），舊 argv 因為第 4 個參數被忽略而仍然可用。
- 舊的遠端 `hook-spool.jsonl` 格式沒變，直接被新的 drain 讀走。
- 使用者不需要做任何事；`hook_port` 留在 config 裡也不會壞。

#### 11.4.8 驗收條件
| # | 內容 | 期望 |
|---|---|---|
| H1 | 遠端 claude bot 送 prompt | 回覆 `source=hook`；daemon log 依序 `pane.agent_status_changed idle` → `remote hook spool replayed replayed=1` |
| H2 | 遠端 7788 被別的程序佔住 | host 仍 `connected`、bot 正常回覆（不再有 `-R`） |
| H3 | daemon 停機時遠端送一輪 | 遠端 spool 多一行；daemon 起來後對帳把它補進對話，不重複 |
| H4 | 同一輪 herdr 報兩次 idle | 只寫一則 assistant 訊息（合併窗 + §6.7 去重） |
| H5 | 遠端沒有 `herdr` 在 PATH | 回合仍在 30 秒內被掃回來（`source=hook`），log 有 `herdr not found; spooled only` |
| H6 | 遠端額度 | 回合結束後 `runs.status_json` 有 `rate_limits`，UI 主機額度條更新 |
| H7 | 舊 argv | 手動用 `hook.sh claude <bot> <token> 7788` 呼叫，第 4 參數被忽略、行為與 3 參數相同 |

### 11.5 目錄選擇器
`GET /api/fs/dirs?host=<name>&path=` 對遠端執行一段 sh：`cd <path> && pwd && for d in */ .[!.]*/; do [ -d "$d" ] && printf '%s\t%s\n' "${d%/}" "$([ -d "$d/.git" ] && echo 1 || echo 0)"; done`，daemon 解析後回傳與本機相同的 JSON（`home` 以 `echo $HOME` 取得；`~` 前綴展開）。隱藏目錄預設略過，`hidden=1` 才列出（本機／遠端一致）。

### 11.6 API 與 UI
- `GET /api/state` 新增 `hosts: [{name, ssh, herdr_session, connected, error?}]`；`projects[].host`（`"local"` 或 host name）。
- `POST /api/hosts {name, ssh, ssh_port?, herdr_session?, remote_path?}` → 寫 TOML、立即嘗試連線、回 `{name, connected, error?}`（v4.3：`hook_port` 仍被接受但忽略，見 §11.4.6）；`DELETE /api/hosts/:name`（需無 project 使用）；`POST /api/hosts/:name/reconnect`。
- `POST /api/projects` 新增 `host?`。
- WS `daemon_status` 改為 `{herdr_connected, hosts: {<name>: {connected, error?}}}`；`host_changed {name, connected, error?}`。
- UI：sidebar Project 標題顯示 host 徽章（本機不顯示）；新增 Project 表單多一個「主機」下拉（本機 + 已設定 hosts），選擇器隨主機切換；新增「主機」管理表單（名稱、ssh 目標、port、session、remote_path；v4.3 移除 hook_port），列出各 host 連線狀態與重連按鈕；host 斷線時該 host 的 bot 燈號為 `disconnected`（灰）。

### 11.7 第一階段不做
遠端密碼 / 互動認證、跳板（ProxyJump 交給 ssh_config）、遠端 transcript 回補、多 daemon。

## 12. grok 支援（v3.6）

第三種 `kind = "grok"`：xAI 的 **grok CLI**（Grok Build，實測 1.0.13，`~/.grok/bin/grok`）。herdr 0.8.2 內建 `grok` agent manifest（bundled 2026.07.16.2），`agent.start {kind: "grok"}` 直接可用，狀態偵測靠 OSC title / OSC 9;4 progress / 畫面規則（附錄 F）。

### 12.1 啟動參數
| 項目 | 注入 |
|---|---|
| `auto_approve` | `--always-approve`（= `--permission-mode bypassPermissions`；使用者的 `~/.grok/config.toml` 若已設 `permission_mode = "always-approve"` 也不衝突） |
| `model` | `-m <model>`（`grok models`：`grok-4.6` 預設、`grok-4.5`） |
| hooks | **無 argv**（見 12.2） |

argv 組合順序與 §2 相同：daemon 旗標 → model → identity.args → bot.args。實測 `agent.start` 後約 3–4 秒 `idle`，本機無 trust 提示（`~/.grok/trusted_folders.toml` 已含該目錄；新目錄可能出現 trust 對話框 → `blocked`，UI 送鍵處理）；畫面有一個「Help improve Grok [Opt out] [Opt in]」遙測 banner，不阻塞輸入。

### 12.2 hook 注入：全域 hooks 檔 + env 分派
grok 1.0.13 的 TUI **沒有**每次啟動注入 hook 的旗標（`--settings` / `--hooks` / `--plugin-dir` 皆 `unexpected argument`；`--plugin-dir` 只在 `grok agent … stdio` 可用）。hook 只能來自 `<GROK_HOME>/hooks/*.json`（全域、永遠信任）、`<project>/.grok/hooks/`（需 trust、會污染使用者 repo）、`config.toml` 的 `[[hooks.<Event>]]`、plugin。因此採 **全域 hooks 檔 + pane env 分派**：

1. daemon 在啟動 grok bot 時（`inject_hooks` 與否皆寫，內容固定、只在變更時覆寫）寫入：
   - `~/.config/agents-manager/grok-hook.sh`（分派腳本，本機版）：
     ```sh
     #!/bin/sh
     [ -n "$AM_BOT_ID" ] && [ -n "$AM_HOOK_TOKEN" ] || exit 0
     exec '<abs agents-managerd>' hook grok --bot "$AM_BOT_ID" --token "$AM_HOOK_TOKEN" --port "${AM_PORT:-7788}"
     ```
     遠端版改為 `exec "$HOME/.config/agents-manager/bots/$AM_BOT_ID/hook.sh" grok "$AM_BOT_ID" "$AM_HOOK_TOKEN"`（v4.3 起遠端不需要埠；舊分派腳本多帶的第 4 個參數會被 `hook.sh` 忽略）。
   - `<GROK_HOME>/hooks/agents-manager.json`：`{"hooks":{"SessionStart":[{"hooks":[{"type":"command","command":"<分派腳本絕對路徑>","timeout":5}]}],"Stop":[…同…]}}`。`GROK_HOME` 取自 identity.env ∪ bot.env（已展開 `$HOME`），缺省 `~/.grok`。
2. pane env 多帶 `AM_HOOK_TOKEN`（§6.2.3）。**`inject_hooks = false` 時不給 `AM_HOOK_TOKEN`**，分派腳本立即 `exit 0`，等同未注入（實測走 `terminal_fallback`）。
3. 使用者自己開的 grok（無 `AM_BOT_ID`）只多付一次 `sh` 啟動的成本，行為不變。hook 失敗對 grok 是 fail-open，且 Stop hook 的 stdout 必須空（JSON 會被當成 decision）——子命令契約 §4.4 已保證。
4. hook 子命令 `hook grok` 與 `hook claude` 相同：payload 從 stdin 讀，POST `/hook/grok`。
5. 刪除 bot 不移除全域 hooks 檔（它不屬於任何 bot；沒有 grok bot 時它是無害的 no-op）。第二階段可在最後一個 grok bot 刪除時清掉。

### 12.3 事件分類與回覆擷取
- `hookrecv::classify("grok")`：`hookEventName`（或 snake_case 副本 `hook_event_name`）
  - `session_start` → Identity（`sessionId`；無 transcript）。**注意：grok 的 SessionStart 延遲到第一次送 prompt 時才觸發**（實測 `agent.start` 後 idle 不會來，prompt 送出瞬間先來 session_start 再來 stop）。
  - `stop` 且 `reason = "end_turn"` 且 `stopHookActive = false` → TurnComplete（`sessionId`、`promptId`、`transcriptPath`、`lastAssistantMessage`）。`reason = "shutdown"`（session 結束時再觸發一次的觀察用 Stop）→ 忽略。
  - `session_end` / 其他 → 忽略。
- 終端備援：grok 回覆是**無標記**的縮排純文字，右側帶 `h:mm AM|PM` 時戳與捲軸字元 `█`。`extract_reply` 對 grok 回 `None`，`clean_screen` 對 grok 另外：去掉行尾 `█` 與右對齊時戳、跳過 `◆ …`（hook / thinking 事件）、`Worked for …  stop [hooks: N]`、`<cwd>  15K / 500K` 標頭、`[stable]`、`Shift+Tab:mode │ Ctrl+.:shortcuts` 頁尾，並把「Help improve Grok … Read Terms and Privacy Policy.」整塊（會依寬度換行）跳過。prompt 回音字元與 Claude 相同為 `❯ `。
- 遠端 host：`REMOTE_HOOK_SH` 對 provider ≠ codex 讀 stdin，grok payload 以 `{` 開頭 → 原樣塞進 spool 行，不需第三種分支；`stop` 的 `reason=shutdown` 不上報狀態（§11.4.2）。

### 12.4 身份隔離
`GROK_HOME`（預設 `~/.grok`）等同 Claude 的 `CLAUDE_CONFIG_DIR`：config.toml、`auth.json`、`sessions/`、`hooks/` 全部跟著走。identity 設 `GROK_HOME=$HOME/.grok-work` 即可用另一個帳號；daemon 會把 hooks 檔寫到該 `GROK_HOME/hooks/`。

### 12.5 資料層
- `bots.kind` CHECK 改為 `('claude','codex','grok')`。SQLite 無法修改 CHECK，舊 DB 以 `bots_new` 重建（`PRAGMA foreign_keys=OFF; legacy_alter_table=ON`，與 projects 的重建同法，並處理中途當機殘留的 `bots_new`）。
- `identities[].kind` 亦允許 `grok`。
- API：`POST /projects/:id/bots` / `POST /identities` 的 kind 驗證改為 `claude | codex | grok`，錯誤訊息 `kind must be claude, codex or grok`。

### 12.6 額度：`/usage` 探測

grok CLI 沒有 `usage` 子命令，也沒有可查額度的 RPC；數字只存在 TUI 的 `/usage` 對話框裡。daemon 因此開一個**用完即丟**的 herdr workspace 探測。

探測跑在**專屬的 herdr session `am-quota`**（daemon 需要時以 `herdr --session am-quota server` 起，永遠不 attach），不是使用者那個 session。兩個理由：

- **寬度**：pane 寬度來自 attach 的 client。使用者終端若只有 32 欄，grok 會把 `/usage` 對話框截掉右緣，百分比整個不見（只剩 `█████░░░…`），怎麼等都解析不出來——這就是「一直顯示背景查詢中」的原因。沒有 client 的 session 用寬預設格線（~180 欄）算版，數字才完整。
- **打擾**：每 30 秒在使用者正在看的 workspace 閃一個 grok pane 不能接受。

流程：

1. `workspace.create`（`focus:false`，label `am-quota-grok`，cwd = 家目錄）
2. `agent.start` kind `grok`、無額外 argv，名稱 `amquota<6碼>`（不在 DB 裡，對帳永遠不會把它當成 bot）
3. `agent.wait` 到 `idle|working|blocked`，再等 3 秒讓輸入列畫好
4. `pane.send_text "/usage"` → 0.8 秒 → `pane.send_keys ["Enter"]`
5. 每 0.9 秒 `pane.read visible 120`，最多 25 秒，直到畫面解析得出額度
6. `workspace.close`（放在 `Drop` 裡，錯誤路徑也會關）

daemon 若在探測中途被砍，workspace 會留下來、裡面的 grok 也還活著。所以 poller 啟動時先 `sweep_stale()`：把 `am-quota` 與本機 session 裡 label 為 `am-quota-grok` 的 workspace 全部關掉（本機那份是為了清掉舊版把探測開在使用者 session 的殘留）。

畫面（去掉框線）長這樣：

```text
Context usage  Usage limit  Session info
Weekly limit (SuperGrok)
████░░░░░░░░░░░░░░░░░░░░░░░░░░  14%
Resets: September 12, 16:28
```

解析規則（`daemon/src/quota_grok.rs`）：

- 標題列 `<window> limit (<plan>)` → window 含 `week` 進 `seven_day`、含 `hour` 進 `five_hour`；括號內是 plan。
- 百分比只認**同時有 `█`/`░` 的列**，所以「Context usage」分頁的百分比不會被誤判成額度。
- `Resets:` 沒有年份，以當下年份補；補完若已過期超過一天則進位到隔年。時間視為本機時區，輸出 RFC3339 UTC。

頻率：啟動時一次，之後每 **30 秒**（使用者指定）；`GET /api/quota?refresh=1` 也會觸發一次。grok 目前只回報週額度，所以 `five_hour` 為 `null`，UI 只畫一條血條。

| # | 內容 | 結果（2026-09-06，本機） |
|---|---|---|
| Q1 | 探測 pane 跑 `/usage` | 對話框在 ~4 秒內出現，`Weekly limit (SuperGrok) 14% Resets: September 12, 16:28` |
| Q2 | `GET /api/quota` 的 `grok` | `{"plan":"SuperGrok","five_hour":null,"seven_day":{"used_pct":14.0,"resets_at":"2026-09-12T08:28:00.000Z"},"source":"grok-usage"}`，與 TUI 逐字相符 |
| Q3 | 額度列 | 三個 kind 同列：claude 2 條、codex 2 條、grok 1 條（`docs/screenshots/227-quota-grok-1400.png`） |
| Q4 | 32 欄的 pane | 對話框截斷成 `█████░░░…`（無百分比）、`Resets: September 12, 16:2`，25 秒逾時；換到未 attach 的 session 後同一份程式碼解析成功 |
| Q5 | 不打擾 / 不殘留 | 每 8 秒取樣 6 次：使用者 session 一路只有 `agents-manager` 一個 workspace，探測 workspace 只在 `am-quota` session 短暫出現；重啟時 `sweep_stale` 關掉了 2 個舊版殘留 |

### 12.7 驗收
| # | 內容 | 結果（2026-09-06，本機） |
|---|---|---|
| G1 | 建 `am-grok` → start | 4 秒 `running/idle`，argv `grok --always-approve`，pane w8:pB；`~/.grok/hooks/agents-manager.json` 與 `~/.config/agents-manager/grok-hook.sh` 已寫入 |
| G2 | prompt「Reply with exactly GROK-OK」 | 7 秒 `completed`，assistant `source=hook` 內容 `GROK-OK`；log 先 `Identity{session_id}` 再 `TurnComplete{promptId, transcriptPath, "GROK-OK"}` |
| G3 | PATCH `{model:"grok-4.5", inject_hooks:false}` → restart | argv `grok --always-approve -m grok-4.5`，pane env 無 `AM_HOOK_TOKEN`；prompt → 10 秒 `completed_fallback`，`terminal_fallback` 內容 `GROK-FALLBACK` |
| G4 | PATCH 還原 → restart → prompt | `source=hook` `GROK-OK-2` |
| G5 | daemon 重啟對帳 | `reconcile: kept active run bot=am-grok`，同一 pane |

### 11.8 驗收
| # | 內容 | 驗收 |
|---|---|---|
| R1 | HostManager | 設定 host `m4p`（`m4p@100.112.229.82`）→ daemon log 出現 remote session ensured、master up、ping ok；`kill` master 程序 → 30 秒內自動重連並對帳 |
| R2 | 遠端 Project / Bot | UI 新增 host、以選擇器選 `/Users/m4p` 下目錄建 Project、新增 claude bot → start → 遇 trust 提示 `blocked` → 按鍵 ↓ Enter → idle |
| R3 | 遠端 hook | 送 prompt → 回覆來源 `hook`（狀態走 herdr 事件、內容走 spool，§11.4）；daemon 停機時遠端 spool 增加一行，重啟後補入。細項見 §11.4.8 |
| R4 | 本機不受影響 | 既有本機 bot 行為與 M1–M8 驗收相同 |
| R5 | 開發測試 | `scripts/dev-sshd.sh` 以使用者權限起 127.0.0.1:2222 的 sshd（不改系統設定），以 `host = "loop"`（ssh 到 127.0.0.1:2222、session `am-loop`）跑 R1–R3 的自動化版本 |

## 13. 專案群組聊天（Project group chat，v3.6）

### 13.1 資料模型
- **一個 Project 就是一個群組**，成員 = 該 Project 底下所有存活（`deleted_at IS NULL`）的 Bot。**不新增 conversation 型別**。
- **群組時間軸** = 成員 Bot 的所有 messages 合併，依 `message.id`（ULID，時間有序）排序，每則附 `bot_id`、`bot_name`。
- 群組發言以既有 §6.3 `prompt()` 送給每個目標 Bot：**每個目標 Bot 各建一個 Turn 與一則 user Message**，`client_request_id = <crid>:<bot_id>` 保持冪等。
- `messages` 表新增欄位 **`group_id TEXT`**（additive migration，舊 DB 啟動時 `ALTER TABLE` 補上；部分索引 `messages_group`）。同一次群組發言產生的 user 副本與「未送達」system 註記共用同一個 `group_id`（= 該次發言的 `client_request_id`）；Bot 的回覆與其他訊息為 NULL。
- 前端把同一 `group_id` 的 user 副本折疊成一則並列出目標（`→ @a, @b`）。

### 13.2 mention 解析（後端為準，前端只做提示）
- `@all` = 專案內所有 Bot（大小寫不敏感）。
- `@<name>` 匹配專案內 Bot 名稱，大小寫不敏感；token 為 `[A-Za-z0-9_-]+`，因此 `@name,`、`@name:`、`(@name)` 的尾隨標點自然被切掉；`@name-` / `@name_` 若整個 token 沒對上會去掉尾隨的 `-`/`_` 再試。
- `@` 必須在開頭或接在非字元（非字母 / 數字 / `_`）之後，`me@example.com` 不算 mention。
- 沒有任何有效 mention → **400 `{error:"no_mention", message, bots:[{id,name,kind}]}`**。
- 送給 agent 的文字**保留原文（含 `@`）**；目標依 Project 內 Bot 順序去重。

### 13.3 不可送的 Bot（略過，不自動啟動）
目標 Bot 沒有 active Run、Run 非 `running`、agent `blocked`、已有 in-flight Turn、或有 `delivery=unknown` 的 Turn → 該 Bot **略過**，列在回應的 `skipped:[{bot_id, bot_name, reason, detail}]`（`reason ∈ not_running | blocked | in_flight | unknown_delivery | conflict | not_found | bad_request | upstream`），並在該 Bot 的 conversation 寫一則 system Message（`group_id` 同），內容如「群組訊息未送達 g-codex：bot 未啟動（不會自動啟動）」。同一 `client_request_id` 重送不會再寫第二則。**絕不**自動啟動。

### 13.4 API（契約細節見 `docs/API.md` §11）
| 方法 | 路徑 | 說明 |
|---|---|---|
| GET | `/api/projects/:id/messages?before=&limit=` | `{project_id, messages:[{...message, bot_id, bot_name}], has_more}`；跨 Bot 合併、以 message id 倒序分頁，回傳正序 |
| POST | `/api/projects/:id/chat` | `{text, client_request_id}` → `{group_id, project_id, sent:[{bot_id, bot_name, turn_id, message_id, delivery}], skipped:[...]}` |

WS：沿用 `message_added`（已含 `bot_id`；`message.group_id` 新增）與 `turn_updated`，前端依 Bot 的 `project_id` 歸入群組。

### 13.5 前端
- sidebar 每個 Project 標題可點（`⌗` 圖示）切到群組視圖；右側標題列顯示專案名、`群組` 標籤、host 徽章與**成員燈號列**（點成員可跳到該 Bot 的單獨對話）。
- 時間軸：Bot 回覆 / system 訊息左上顯示 Bot 名稱徽章（依 kind 配色）；user 訊息折疊顯示 `→ @a, @b`；各 Bot 回覆沿用既有 Markdown 氣泡；每個仍在回覆中的成員各一個 typing 指示。
- 輸入框：輸入 `@` 彈出成員與 `all` 的自動完成（↑ / ↓ 移動、Enter / Tab 選取、Esc 關閉）；沒有 mention 時送出鈕 disabled 並提示；有 mention 時列出收件者，並標示目前無法接收、將被略過的成員。
- Composer 鎖定規則：**專案內至少一個 Bot 可送就允許**（單一 Bot 的 §6.3 規則逐一判斷）。

### 13.6 通知
群組視圖打開時不做額外通知；未打開時 sidebar 的 Project 標題顯示未讀計數（assistant / system 訊息，前端記憶體，開啟群組視圖即歸零）。

### 13.7 驗收（2026-09-06，獨立 daemon `AM_DATA_DIR=/tmp/am-group`、port 7799、session `am-group`）
| # | 內容 | 結果 |
|---|---|---|
| G1 | 兩個 Bot（g-claude / g-codex）start 後 `POST chat {"text":"@all Reply with exactly GROUP-OK"}` | `sent` 兩筆 `delivery=ok`；兩個 Bot 都回 `GROUP-OK`（`source=hook`）；`GET projects/:id/messages` 合併時間軸正確（user 副本各一、`group_id` 相同） |
| G2 | `@g-claude 只有你…` | 只有 g-claude 收到（`sent` 一筆），回 `ONLY-CLAUDE` |
| G3 | 停掉 g-codex 後 `@all` | `skipped:[{bot_id:g-codex, reason:"not_running"}]`，g-codex conversation 出現一則 `group_id` 相同的 system 訊息；同一 crid 重送回同一 `turn_id`、system 訊息不重複 |
| G4 | 沒有 mention | `400 {error:"no_mention", bots:[…]}` |
| G5 | `limit=3` / `before=<id>` | `has_more` 與游標分頁正確 |

## 14. 每台主機各自的額度（v3.9）

標題列的額度條原本只有一組數字，而且不管在看哪一台主機都是同一組：codex 走本機 `codex app-server`、
claude / grok 的 `/usage` 探測開在本機、遠端 bot 的 statusLine 也直接蓋在裸的 `claude` key 上。
於是**遠端 bot 的數字會覆蓋本機的那一列**，而且沒有任何地方看得到遠端主機還剩多少。

規則：**額度屬於它被讀到的那台主機**。標題列一次只顯示一台——預設本機，點進 ssh 主機上的 bot 或
專案時就換成那台（使用者決定：切換顯示，不並列）。

### 14.1 Key 與資料形狀
- map key：本機維持裸的 `claude` / `claude:cc1` / `codex` / `grok`；遠端主機加自己的名字當前綴：
  `m4p/claude`、`m4p/claude:cc1`、`m4p/codex`、`m4p/grok`（與 `GET /api/models` 的 `host/kind` 快取同形）。
  host 名的字集是 `[a-z][a-z0-9_-]{0,31}`，不含 `/` 或 `:`，所以這個 key 永遠拆得回來。
- 每筆 quota 多一個 `host` 欄位（`local` 或 `hosts[].name`）。`quota::set(app, host, base_key, q)` 統一
  蓋章：呼叫端只給「裸 key」，key 前綴與 `host` 欄位都由它組，沒有哪個路徑能把遠端讀數存進本機那列。
- 主機被刪掉時連它的額度列一起刪；`GET /api/quota` 的快照也會丟掉不屬於任何現存主機的 `<host>/…` key
  （否則下游會把它當成本機的裸 key 讀）。

### 14.2 三個來源都跟著主機走
- **codex**：`codex_rpc(app, host, "account/rateLimits/read")` — 本機直接跑，遠端走既有的 `ssh_exec_path`。
- **claude statusLine**：hook 進來時查 `bot_host(bot_id)`，寫進那台主機的列。遠端 bot 的 hook 本來就會
  經反向通道回到 daemon，所以遠端只要有 bot 在對話就有即時數字，不必等探測。
- **claude / grok `/usage` 探測**：§12.6 那套流程原封不動，只是換一個 herdr client。
  - 本機：仍是專屬的 `am-quota` session（不打擾使用者、版面夠寬）。
  - 遠端：借 **daemon 自己在那台上的 named session**（`agents-manager`）。遠端只有一條被轉發的 socket
    （§11.3），再開一個 `am-quota` session 等於要多一條轉發；而那個 session 本來就是 daemon 開的、
    不是使用者的，探測 workspace 的 label 仍是 `am-quota-claude*` / `am-quota-grok`、agent 名仍是
    `amquota<6碼>`（不在 DB 裡），對帳不會把它當 bot。`sweep_stale()` 現在會掃本機 `am-quota` 加上
    每一台已連線主機的 session。
  - 探測的 cwd 與 identity env 裡的 `~` 都用**那台主機的 `$HOME`**（`HostConn::home()`）。
  - `agent.wait` 的條件是 `idle | blocked`：卡在對話框時 `blocked` 幾秒就回來，只等 `idle` 得先燒完
    90 秒逾時。`blocked` 時**先讀畫面再決定按鍵**：認得出工作區信任對話框（`trust this folder` /
    `do you trust`）就送 Down + Enter，其餘才送裸的 Enter。順序很重要——那個對話框的游標停在
    **No, exit**（`crate::trust` 的老問題），2026-09-06 在 m4p 實測：先送 Enter 等於直接把 claude
    關掉，之後每一個鍵都打進 shell（畫面留下 `zsh: no such file or directory: /usage`）。答過一次
    之後 claude 自己記下信任，後續探測就直接 idle。

### 14.3 輪詢
三個 poller 的節奏不變（codex 5 分、claude 60 秒、grok 30 秒），每一輪對 **`local` 加上每一台已連線的
遠端主機**各跑一次（使用者決定：所有已連線遠端都持續輪詢，不只在檢視時），**同一輪的各主機併發**
（`JoinSet`）：一次探測要數十秒，序列跑三台會把本機的週期拉成好幾分鐘。斷線的主機直接跳過，
連上後下一輪自然補回來。`probe_lock` 從單一全域鎖改成 **per host**，所以併發不會互相踩；同一台上的
多個 identity 仍是一個一個探（它們共用 pane）。`GET /api/quota?refresh=1` 則維持依序，回應才好預期。

### 14.4 API / WS
- `GET /api/quota?refresh=1[&host=<name>]`：`refresh` 預設重讀 `local` + 每一台已連線主機；給 `host=`
  就只重讀那一台（不存在 → 404）。回應永遠是完整的 map（每台主機都有三個基本 kind 的 key，沒資料為 `null`）。
- WS `quota_updated` 的 `kind` 是完整 map key（含 `<host>/` 前綴），另外多帶一個 `host` 欄位。

### 14.5 UI
- 標題列額度條吃一個 `host`：ChatPanel 用該 bot 專案的 host、群組聊天用 Project 的 host、Team 面板用
  Team 專案的 host，沒選任何東西時是本機。
- 遠端時在條的最左邊掛一個主機名牌（`.quota-host`）；**本機不掛**——本機是預設狀態，多一個「本機」
  標籤只會佔掉標題列寬度。每個 gauge 的 tooltip 一律以主機名開頭，popover 標題寫「本機額度」/「m4p 的額度」。
- 側欄 bot 列的 critical 警告改讀該 bot 所在主機的列；Team 的 `quota_stop_pct` 閘門讀 Team 專案主機的列。

### 14.6 驗收（2026-09-06，mock backend；`node scripts/demo-quota-host.mjs`）
| # | 內容 | 結果 |
|---|---|---|
| H1 | 選本機 bot | 額度條無主機名牌，cc0 5h 82% / cc1 5h 15% / codex 5h 37%（`docs/screenshots/340-quota-host-local.png`） |
| H2 | 新增 m4p、開遠端 project + bot 並選進去 | 條上出現 `m4p` 名牌，數字換成該台的 cc0 5h 54% / cc1 5h 93% / codex 5h 4%（`341-quota-host-remote.png`） |
| H3 | 展開 popover | 標題「m4p 的額度」，四列都是 m4p 的（`342-quota-host-remote-pop.png`） |
| H4 | 切回本機 bot | 數字與名牌回到 H1 的狀態（`343-quota-host-back-to-local.png`） |
| H5 | 深色 / 1040 寬收合 | 名牌與收合後的單一窗口都正常（`344-quota-host-remote-dark.png`、`345-quota-host-remote-narrow.png`） |
| H6 | 移除主機 | 該台的 `<host>/…` 列從 map 中消失，條回到本機 |

真後端（本機 daemon + `m4p@100.112.229.82`，2026-09-06）：

| # | 內容 | 結果 |
|---|---|---|
| H7 | 本機三個來源 | `claude` 5h 64% / 7d 26%（`claude-usage`）、`codex` 5h 100% / 7d 34%（`codex-app-server`）、`grok` 週 54%（`grok-usage`），每筆 `host:"local"` |
| H8 | 加入 m4p 後 `GET /api/quota` | 立刻多出 `m4p/claude`、`m4p/codex`、`m4p/grok` 三個 key（尚未輪詢到 → `null`） |
| H9 | `?refresh=1&host=m4p` 的 codex | `m4p/codex` = 5h 0% / 7d 34%、plan `plus`、`host:"m4p"`，走 ssh 的 `codex app-server` |
| H10 | m4p 沒裝 grok | `grok not installed; grok quota stays null host=m4p`，`m4p/grok` 保持 `null`（不重試、不報錯） |
| H11 | 遠端 claude `/usage`（照 §14.2 的鍵序在 m4p 的 herdr session 手動重跑） | Down + Enter 通過信任對話框後，`/usage` 畫出 `Current session 0% used` / `Current week (all models) 33% used`，正是 `parse_claude_usage` 吃的格式；探測 workspace 收乾淨 |

> H11 沒有走 daemon 跑完：驗證當下本機已經有一個正式 daemon（:7788）在管同一台 m4p，兩個 daemon 會搶
> 同一條 ssh master 與 `/tmp/agents-manager-<uid>/m4p.sock`（`hosts::short_dir()` 只用 uid 命名），
> 測試 daemon 的探測進行到一半 master 就被踢掉（`ssh master exited`）。要端到端跑遠端 claude 探測，
> 得先停掉另一個 daemon。

## 15. herdr 這一側的記憶體（v4.0）

左上角那一格回答的是「這些 agent 現在花我多少記憶體」。

### 15.1 量什麼
整棵 **herdr 進程樹**：`herdr` 本身 ＋ 它底下的 pane 與 agent CLI。錢花在 pane 裡那個 `claude`
上，只報 herdr daemon 自己沒有意義。樹根 = 執行檔名等於 `herdr` 的 process（argv 裡剛好有這個
字的不算，例如 `grep herdr`），herdr 底下再開 herdr 只算一次。每台主機每 15 秒一次
`ps -Awwo pid=,ppid=,rss=,args=`（遠端走既有 ssh master），變化超過 1 MiB 才推 `mem_updated`。
量不到的主機用 `error` 回報而不是從清單消失。端點見 `docs/API.md` 的 `GET /api/mem`。

### 15.1a 這台機器還剩多少（2026-09-12）
使用者：「除了已用量，也要能夠 show 出剩餘 ram 量。」herdr 樹佔 7G 這個數字回答不了「還能不能
再開一顆 bot」——那要看整台機器還剩什麼，而剩下的多半是被瀏覽器與系統吃掉的，不在 §15.1 的樹裡。

同一次取樣、同一個 snapshot、同一個 `mem_updated` 事件多帶一節 `hosts[].machine`：
`{"total_bytes":…,"available_bytes":…}`。取法依平台，一次 shell 往返（遠端就是同一次 ssh）：

- Linux：`/proc/meminfo` 的 `MemTotal` / `MemAvailable`。
- macOS：`sysctl -n hw.memsize` 拿總量，`vm_stat` 的 `Pages free + inactive + speculative +
  purgeable` × page size 當可用量——inactive／purgeable 是核心隨時能回收的快取，算成「已用」
  會讓人以為記憶體早就見底。

**`available` 不是 `total −（我們用掉的）`**：那台機器上還有瀏覽器、編輯器、系統自己。兩種輸出都
認不出來（指令不存在、被截斷）就回 `null`，UI 只顯示已用量——少一個數字沒關係，猜一個會誤導。
舊格式（沒有分隔線、只有 `ps`）照舊算得出 herdr 的數字。

UI：左上那格變成「已用 · 剩 N」，剩餘低於 15% 時整格轉警示色；展開的明細最上面一行寫
「這台機器 剩 N / 共 M（已用 …，其中 herdr 樹 …）」。

### 15.2 展開看程序 / 砍程序（2026-09-08）
一台機器底下常常有十幾個 `claude`，其中一半是使用者自己開的 pane 或舊的 `--resume`，
不是 AG Man 管的 bot——但從那一個總數看不出來哪些可以砍。所以那一格可以展開成清單。

**owner 判定完全讀 process 的環境變數，不讀我們自己的帳本**：daemon 起 bot 時注入
`AM_BOT_ID`（`lifecycle.rs`），herdr 對每個 pane 注入 `HERDR_PANE_ID`，而環境會被子孫繼承，
所以 pane 底下好幾層的 CLI 一樣帶得到。這樣連「這個 daemon 開機前就在跑」的程序也判得對，
帳本永遠做不到這件事。macOS 用 `ps -Ewwo pid=,args=` 讀得到同一個 user 的環境，Linux 讀
`/proc/<pid>/environ`。

| owner | 條件 | UI |
|---|---|---|
| `bot` | 環境有 `AM_BOT_ID`（bot 已刪也算，`bot_id` 照回） | 「停止 bot」 |
| `pane` | 只有 `HERDR_PANE_ID` | 「結束」→ 再按一次「強制」 |
| `herdr` | 執行檔就是 `herdr` | 不列、不可砍 |
| `unknown` | 兩個都讀不到 | 同 `pane` |

清單只列 `claude` / `codex` / `grok` / `node` / `bash` / `zsh` / `sh` / `fish` 且
`subtree_bytes ≥ 8 MiB` 的；其餘（幾十個 node worker、短命的 helper）併進父程序的
`subtree_bytes`，不把清單變成 process explorer。排序用 `subtree_bytes`，因為使用者要的答案是
「砍掉這個能省多少」。

「自己開的 pane wM:pB」對使用者是亂碼：十個 claude 哪個是哪個，得看畫面才知道。所以 owner 那格
點得開，開一個跟 BlockedModal 同寬的視窗顯示那個 pane 現在的畫面（`GET /api/mem/processes/pane`，走 herdr
`pane.read visible`，不需要那個 pane 是我們開的），每 2 秒重讀；只讀、純文字，
不給打字——這裡是決定砍不砍的地方，要操作它就去 herdr。bot 那幾列不給看，它有自己的終端分頁。

砍之前**一定重新取樣**再判定，不信前端送來的那一列：pid 會被回收，過期的一列不能讓 `kill`
逃出 herdr 樹。不在樹裡 → 400，`herdr` 本身 → 400，`owner=bot` → 409（bot 走既有的
`POST /bots/{id}/stop`，那條路才會記錄停止）。砍完立刻取樣並推一次 `mem_updated`。

## 16. 從 shell 認出來的身份 cc0～cc6（v4.1）

多帳號的人本來就已經把帳號寫在 shell 裡了：

```sh
# ~/.zshrc
alias cc0='claude --dangerously-skip-permissions'
alias cc1='CLAUDE_CONFIG_DIR=$HOME/.claude-cc1 claude --dangerously-skip-permissions'
alias cc2='CLAUDE_CONFIG_DIR=$HOME/.claude-cc2 claude --dangerously-skip-permissions'
```

所以 daemon 直接讀它，`cc0`…`cc6` 不必再手寫一份 `[[identities]]`。

### 16.1 怎麼讀
偵測跟在既有的工具偵測（§12 的 `tools` 那一支）後面，同一個腳本、同一次 ssh：

```sh
al=$( "${SHELL:-/bin/sh}" -lic 'alias' 2>/dev/null )   # 走登入 shell：zshrc / bashrc / 被 source 的都算
[ -n "$al" ] || al=$(cat "$HOME/.zshrc" 2>/dev/null)    # $SHELL 不是互動 shell 時的退路
printf '%s\n' "$al" | grep -E "(^|[[:space:]])(alias[[:space:]]+)?cc[0-6]="
```

解析規則（`tools::parse_shell_identities`）：

- 名字必須正好是 `cc0`…`cc6`；`cc`、`cc7`、`ccx` 不算。
- 命令裡必須真的跑 `claude`（或 `…/claude`），否則跳過——`cc1` 指向別的東西不是我們的事。
- 只取**開頭**的 `CLAUDE_CONFIG_DIR=`（前面只能是其它 `VAR=value`）。`claude --settings CLAUDE_CONFIG_DIR=…`
  這種寫在旗標裡的不算。值到第一個未引用的空白為止，引號會剝掉。
- **旗標一律不取**（使用者決定）：`--dangerously-skip-permissions` 這類授權旗標由 daemon 的
  `auto_approve` 決定，兩邊各自注入只會打架。所以 `args` 永遠是空的。
- 同名重複取最後一個，跟 shell 自己解析 alias 的規則一致。
- 沒有 `CLAUDE_CONFIG_DIR` 的（典型是 `cc0`）→ **env 為空**的身份，也就是預設帳號；額度列本來就把
  它折到裸的 `claude` key 上（§14.1）。

### 16.2 每台主機各一份
發現到的身份**不寫回 `config.toml`**（使用者決定）：`cc1` 在本機是 `~/.claude-cc1`，在 m4p 是
`~/.claude-ccompany`——同一個名字、不同帳號，寫成全域設定就會在遠端跑錯帳號。它們跟著該主機的
偵測結果走：

- `hosts[].shell_identities`：那台主機讀到的 `ccN`（`cc0`…`cc6` 順序），每次偵測重讀。
- `hosts[].identities.<name>`：登入狀態，多兩個欄位 `source`（`config` / `shell`）與 `config_dir`
  （已用**那台**的 `$HOME` 展開，顯示用）。
- 合併規則只有一條，`tools::identities_for_host()`：**config.toml 的 `[[identities]]` 先，同名的
  shell 身份讓位**。手寫的永遠贏得過猜出來的。

### 16.3a 主機層一鍵登入

身份列可呼叫 `POST /api/hosts/{name}/identities/{identity}/login`。daemon 在該主機 manager session 開一個
臨時 host-shell pane，將 identity env 以該主機 `$HOME` 展開後，執行：`claude /login`、`codex login` 或
`grok login`。pane 的 terminal snapshot 是 UI 顯示 device code / URL 的唯一通道；這些內容不進 daemon log、
WebSocket 事件或 team timeline。

登入 CLI 結束時（成功或失敗）daemon 重新做該身份的登入探測，再關閉臨時 pane；若建立或登入失敗也走同一條
清理路徑。若無法判定，`logged_in` 必須是 `null`，並以 `reason` 說明 CLI 缺失、指令失敗或輸出解析失敗，
UI 顯示「未知」而不是已登入。

### 16.3 誰在用
| 用途 | 之前 | 現在 |
|---|---|---|
| 啟動 bot 的 pane env / args | `cfg.identities` | `identities_for_host(bot 的 host)` |
| `POST /projects/:id/bots`、`PATCH /bots/:id`、開團的 role 驗證 | 全域查名字 | 在**該 bot / 專案的 host** 上查 |
| claude 額度探測的 targets（§14） | `cfg.identities` | 該主機的清單，各自一個 `claude:<name>` 列 |
| UI 身份選項 / 身份面板 / 側欄計數 | `state.identities` | config ∪ 該主機的 shell 身份（標得出來源） |

`[[identities]]` 本身沒有變：仍可手寫、可從 UI 新增與刪除，也仍然是唯一可編輯的那種。從 shell 認來的
是唯讀的——要改就去改那台主機的 alias。

### 16.5 alias 定期重讀
偵測結果本來只在 daemon 啟動、主機連線、或按「重新偵測」時更新，改完 `~/.zshrc` 要等重啟才看得到新的
`ccN`。現在 daemon 每 60 秒對每台可達的主機只跑 alias 那半段 probe（一個登入 shell，不碰 CLI，跟 RAM
取樣一樣便宜），結果跟快取的 `shell_identities` 比對；**只有名單或 `CLAUDE_CONFIG_DIR` 變了**才觸發一次完整
偵測（含登入探測）並推 `host_changed`。沒變就什麼都不發。還沒做過第一次偵測的主機不在此列，等它自己的
連線偵測。

### 16.4 探測節流
`cc0`…`cc6` 全開的話，一台主機最多 7 個 claude 帳號，每個帳號的 `/usage` 探測要開一次 TUI（~25 秒），
一輪 60 秒根本跑不完。所以額度探測（§14.3）多兩個跳過條件：

- 該身份的列在 **`CLAUDE_POLL` 內剛被 statusLine 更新過** → 不探（有 bot 在對話的帳號本來就有即時數字）。
- 工具偵測說該身份在這台**沒有登入**（`logged_in == false`）→ 不探（它只會停在登入畫面，把 25 秒的
  對話框逾時燒掉）。等哪次偵測看到它登入了，下一輪自然恢復。

grok 的 `/usage` 探測（§12）套同一套（2026-09-12）：工具偵測說 grok 在這台沒登入就不探；探測失敗
（畫不出額度列、trust 提示、agent.wait 逾時）後把該主機停 **5 分鐘**再試，不然每 30 秒都要
`workspace.create` → `agent.start` → 最長 60 秒 `agent.wait` → 25 秒讀畫面 → 關掉。`GET /api/quota?refresh=1`
不受這個節流影響，永遠真的探一次。

### 16.5 驗收
mock（`node scripts/demo-identity-shell.mjs`，需 `VITE_MOCK=1 npx vite --port 5311`）：

| # | 內容 | 結果 |
|---|---|---|
| I1 | 環境設定 → 身分 | config 的 cc0 / cc1 可刪；虛線下方是「從 shell 認來的」cc2 → `/Users/me/.claude-cc2`，唯讀（`docs/screenshots/350-identities-shell-local.png`） |
| I2 | 加一台 m4p | 同一個 `cc2` 多出一列，指到 `/Users/m4p/.claude-ccompany`——同名不同帳號（`351-identities-shell-two-hosts.png`） |
| I3 | Bot 設定的身份選項 | `不指定身分 / cc0 / cc1 / cc2`，cc2 的 tooltip 寫明「來自本機的 shell alias（ccN），不是 config.toml」與登入的帳號（`352-bot-settings-identity-options.png`） |
| I4 | 額度列 | 本機 cc0 / cc1 / cc2 三條，切到 m4p 後同樣三條但數字是那台的（`340`〜`345`，§14.6） |

真後端（本機 daemon，`~/.zshrc` 有 cc0 / cc1 / cc2）：

| # | 內容 | 結果 |
|---|---|---|
| I5 | `GET /api/state` 的 `hosts[0].identities` | `cc0`（無 `config_dir`，已登入）、`cc1`（`/Users/…/.claude-cc1`，**未登入**）、`cc2`（`/Users/…/.claude-cc2`，已登入，帳號與 cc0 不同），三個都是 `source: "shell"` |
| I6 | 額度 | `claude:cc2` 由 `/usage` 探測填上；未登入的 `cc1` 依 §16.4 被跳過，不再每分鐘燒一次 25 秒逾時 |

### 16.6 收編的子 agent 身分怎麼判定（v4.4，2026-09-11）
子 agent 的 pane 是**母 bot 開的**，不是 daemon 開的：`herdr pane split --env
CLAUDE_CONFIG_DIR=$HOME/.claude-cc2` 就能把小孩放到另一個帳號。收編（§6.5）那一刻 daemon 手上只有母
bot 那一列，所以 `bots.identity` 一直是直接抄母 bot 的——小孩燒的額度因此記到錯的帳號（側欄與
`/api/quota` 都是），母帳號看起來比實際緊、子帳號比實際鬆。

herdr 的 `pane.process_info` 回 argv / cwd / **pid**，不回 env，所以帳號只能跟作業系統要：

1. 補 model / effort 的同一次 `pane.process_info`（§4.4a 旁邊那條）挑出 pane 的 CLI 行程，拿它的 `pid`。
2. `ps eww -p <pid>` 讀那個行程的環境變數（本機直接跑，遠端同一句走 `HostConn::ssh_exec`），取
   `CLAUDE_CONFIG_DIR` / `CODEX_HOME` / `GROK_HOME`（依 `bot.kind`）。
3. 拿那個目錄回頭比對 `identities_for_host(host)`（§16.2，config 的 `[[identities]]` 排在 shell 認來的
   `ccN` 前面，所以同一個目錄被兩邊都指到時手寫的贏）。比對前把 `~` / `$HOME` 用**那台主機**的 `$HOME`
   展開，並且正規化結尾斜線與 macOS 的 `/private` 前綴。對得上就寫回 `bots.identity`。

界線跟 model / effort 只差一點，而且是刻意的：

- **只動 `managed_by='child'`**。其它 bot 的 `identity` 是使用者設定、而且會被投影寫回 `config.toml`，
  daemon 從行程猜一個值蓋上去等於改使用者的檔案。SQL 的 `WHERE` 也帶著 `managed_by = 'child'`。
- model / effort 是**只補不改**（argv 看不到之後在 TUI 打的 `/model`）；identity 是**補，也改**——
  子 agent 的 identity 從來不是使用者設的，那是收編當下抄來的值，蓋掉它是在修我們自己抄錯的東西。
- **預設帳號也要認得**。沒有那個變數、或變數指向 CLI 自己的預設目錄（`CLAUDE_CONFIG_DIR=~/.claude`、
  `CODEX_HOME=~/.codex`），就是預設帳號＝空 env 的身份（`cc0`）；不只一個時照第 3 點 config 優先、第一個贏。
  （2026-09-11：母 pane 明寫 `CLAUDE_CONFIG_DIR=~/.claude` 開出來的 cc0 小孩，選單一直顯示母 bot 的 cc1。）
- **永遠不清成 NULL**。env 讀不到、目錄沒有任何身份認領、或該 kind 沒有空 env 的身份，一律維持現狀：
  抄來的值可能是對的，NULL 一定是錯的。
- **一個 pane 只問一次作業系統**。重連會重播一串 `pane.agent_detected`、每個都排一次 reconcile，每次
  每個小孩一次 `ps`（遠端就是一次 ssh）正是 §11.4.7 already 踩過的風暴。行程活著就不會換帳號，所以問過
  就記著；讀不到、或那台主機還沒偵測出任何同 kind 的身份（開機時 reconcile 可能跑在 §16.1 的 alias
  偵測前面）不算問過，下一輪再問。

## 17. claude 的 `--effort`（v4.1）

claude 2.1 起有 `--effort <low|medium|high|xhigh|max>`（`claude --help`），所以 `bot.effort`
不再只對 codex / grok 有效：

- `config::efforts_for_kind("claude")` = 那五級；`normalize_effort` 不再把 claude 一律清成 `None`
  （不合法的值照樣 400，例如 codex 的 `none` 或 TUI 才有的 `ultracode`）。
- 啟動注入 `--effort <level>`（小寫），位置與其它 kind 的強度相同（daemon 旗標 → persona → model
  → effort → identity.args → bot.args）。
- `GET /api/models` 的 claude 靜態清單每個 alias 都帶同一組 `efforts`——claude 沒有 codex
  `model/list` 那種 per-model 清單，所以 UI 的強度列不標「依 <模型>」。`default_effort`
  倒是會依身份而變，見 §17.1。
- **可以當場套用**：`/effort <level>` 帶參數就直接生效（2.1.263 實測，畫面回
  `Set effort level to low (saved as your default for new sessions)`）；不帶參數的 `/effort`
  才是那條拉桿（`←/→ to adjust · Enter to confirm`）。所以 `PATCH /bots/:id {effort}` 走
  `apply_live_setting` 送 `/effort <level>` 並回 `needs_restart: false`，條件與 grok 相同
  （只改這個欄位、run 在跑且不忙、不是清成 CLI 預設）。
- **副作用**：那行 `saved as your default for new sessions` 是 claude 自己的行為——用
  `/effort <level>` 會順手把它存成該帳號之後新 session 的預設強度。TUI 的拉桿有「按 `s` 只
  套用這一次」，但那條路要靠方向鍵定位，送不出確定的值，所以 daemon 用帶參數的形式。

### 17.1 「預設」提示從哪來（v4.2）

「不指定強度」在 codex / grok 一直都會標出那個模型自己回報的預設值（`預設（中）`）；claude
之前只寫「預設」——不是不能顯示，是 claude 根本沒有這種 per-model API，那個值其實是**帳號的
`settings.json`**：

```json
{
  "effortLevel": "high",
  "modelSettings": { "claude-opus-5": { "effortLevel": "low" } }
}
```

- `effortLevel`：全域預設，`claude --effort` 沒帶值時整個帳號的落點。
- `modelSettings.<真實 model id>.effortLevel`：per-model 覆寫。**真實 id 不是我們啟動時用的
  alias**（`claude --model opus` 實際跑的是 `claude-opus-5`，`/status` 的 `Model:` 行會印出這個
  對照），所以比對用**子字串**——`per_model` 的 key 含有 `opus`/`sonnet`/`haiku`/`fable` 哪個字
  就算命中。驗證過本機與 m4p 目前存在的每一個 key 都吻合這個規則；不吻合的話就是沒有提示，不會
  猜錯。
- 兩者都沒有、檔案讀不到、或不是合法 JSON → 落回 **claude 自己的內建預設 `high`**（v4.2 初版
  在這裡回過 `null`——查了官方文件才發現查錯了，見下方 E10 那條）。
- 走身份：`GET /api/models?kind=claude&host=&identity=` 的 `identity` 決定讀哪個
  `CLAUDE_CONFIG_DIR/settings.json`（`identities_for_host`，SPEC §16——shell 認來的 `ccN` 一樣
  適用）；不指定或指定到不存在 / 非 claude 的身份，一律退回預設帳號的 `~/.claude/settings.json`。
  本機讀檔、遠端經 ssh `cat`，與 grok 的 `models_cache.json` 走同一套模式。
- 快取 key 因此多一段身份（`{host}/{kind}/{identity}`，10 分鐘 TTL）：換身份不會沿用上一個身份
  的提示。
- UI 的 tooltip 把來源講清楚——「不帶 --effort（帳號目前設定 高）」——不寫「模型預設」，因為
  那個值換一個身份就不一樣，不是模型本身內建的。

實測（2026-09-06/07，claude 2.1.263，本機與 m4p 同版）：

| # | 內容 | 結果 |
|---|---|---|
| E1 | `claude -p --effort high --model haiku` | 正常回覆 |
| E2 | `claude -p --effort bogus` | `Warning: Unknown --effort value 'bogus' — ignoring it and using the default effort. Valid values: low, medium, high, xhigh, max.` — 只是警告，不會讓 run 掛掉 |
| E3 | TUI 打 `/effort`（不帶參數） | 出現拉桿：`low medium high xhigh max ultracode`（`ultracode` = xhigh + workflows，只有 TUI 有，CLI 不吃），`←/→ to adjust · Enter to confirm · s for this session only` |
| E4 | TUI 打 `/effort low` | 直接套用：`⎿ Set effort level to low (saved as your default for new sessions): …`，狀態列變成 `○ low · /effort`；再開拉桿 ▲ 停在 low |
| E5 | UI（v4.1） | Bot 設定的「強度」列對 claude 出現（預設 / 低 / 中 / 高 / 最高 / Max）並標「執行中改會即時套用，不用重啟」（`docs/screenshots/353-claude-effort.png`） |
| E6 | `/status` 對照 alias → 真實 id | `Model: sonnet (claude-sonnet-5)`；本機 `~/.claude/settings.json` 另存了 `claude-opus-5` 的覆寫 |
| E7 | `GET /api/models?kind=claude`（本機，無 identity） | `opus→low`（覆寫）、`sonnet/haiku/fable→high`（全域） |
| E8 | 同上，`identity=cc1`（`~/.claude-ccompany`） | 四個 alias 都是 `medium`（該帳號只有全域，沒有 per-model 覆寫） |
| E9 | 同上，`identity=` 不存在的名字 | 退回預設帳號的結果，與 E7 相同 |
| E10 | mock（`node scripts/demo-effort-default-hint.mjs`） | 不指定身份 `預設（高）`；選 Opus 後變 `預設（低）`；切到 cc1 變 `預設（中）`；切到 cc2（沒設過）落回 `預設（高）`（`docs/screenshots/362`〜`365`） |
| E11 | claude 官方文件 `code.claude.com/docs/en/model-config`「Choose an effort level」 | 逐字：「`high` \| Balances token usage and intelligence. **The default on every model except Opus 4.7**」（Opus 4.7 預設 `xhigh`）——`opus`/`sonnet`/`haiku`/`fable` 四個 alias 都不是 Opus 4.7 |
| E12 | 真機驗證 E11：一個 `settings.json` 從沒碰過 effort 的乾淨帳號（cc2），開 claude 直接切到 Sonnet | 開場橫幅印 `Sonnet 5 with high effort`，狀態列 `● high · /effort`，`/effort` 拉桿 ▲ 停在 `high`——證實 §17.1 那個 `null` 是查漏了，已改回 `high` |
| E13 | 同帳號（cc2）換成 `--model haiku` | `/effort`（不帶參數）一樣開得出拉桿，五級都在，▲ 停在 `high`——haiku 支援 effort，且預設同樣是 `high`（E11 的官方表格沒列 haiku，但實機行為以這條為準） |

## 18. 總管（AGM）運維規範（2026-09-12）

AGM 的運維職責以本節為準，不靠任何 bot 的記憶。persona 只是同一份規則的執行期投影
（§18.6），launchd 腳本是它的實作；三者不一致時**以實際腳本行為為準**，然後把本節改對。

### 18.1 開發用 dev server（5173）

- **正式位址** `http://<本機>:5173`，`--strictPort`（搶不到就失敗，不要默默換 port——使用者手機上的書籤是寫死的）。
- **來源是一棵只跟 origin/main 的乾淨 worktree**：`/Users/m4p/project/agents-manager-main`
  （`git worktree`、detached HEAD）。看門狗每輪先 `git fetch && git reset --hard origin/main`——那棵樹
  沒有任何人未提交的改動，reset 是安全的；`web/bun.lock` 變了才 `bun install` 並重啟 vite，只有原始碼
  變就靠 vite 自己的 HMR。**5173 看到的＝已經合併進 origin/main 的事實**，推上去重新整理就看得到。
  由來（2026-09-12 使用者「dev 也該要馬上 change」）：原本它吃共用工作樹 `~/project/agents-manager`，
  那棵樹同時有二十幾個檔是其他 agent 未提交的 WIP，既不能 pull（會動到別人的東西）也就永遠追不上
  origin/main——使用者在手機上看到的一直是好幾十顆 commit 以前的畫面。**共用工作樹從此不再被 5173 用。**
- **bot 要驗自己還沒提交的改動，用自己的 port**（`5188` 那類 `VITE_MOCK=1`、或任何沒被占用的 port，
  自己起自己收），不要再把改動丟進 5173 等別人看——5173 是使用者的視窗，不是誰的工作區。
- **runtime 是 node，不是 bun**：`node web/node_modules/vite/bin/vite.js`。bun 1.3.14 交給 HTTP
  upgrade handler 的 socket 沒有 Node 的 `destroySoon`，vite 代理在 upgrade 回應結束時會呼叫它
  （`proxyRes.on('end') → socket.destroySoon()`），於是**正式 daemon 一重啟、代理目標斷線，vite 整個
  crash**（2026-09-12 實例：`TypeError: socket.destroySoon is not a function`，Bun v1.3.14）。
  實測差異：node v22 的 upgrade socket 是 `Socket` 且 `destroySoon` 可呼叫，bun 同一支測試是
  `undefined` 並丟出同一則 TypeError。**bun 只用來 build（`bun run build`）與裝套件，不用來跑 dev server。**
- **必綁 `--host 0.0.0.0`**：使用者從手機／LAN／Tailscale 上的裝置存取，綁 `127.0.0.1` 只有這台機器連得到。
  `web/vite.config.ts` 也設了 `server.host: true`，所以**手動 `npx vite` / `bun run dev` 起的也會對外**，
  不會再出現「某人用預設值起了一顆只聽 loopback 的 vite，占著 5173 但手機連不到」。看門狗仍顯式帶
  `--host 0.0.0.0`，不倚賴設定檔（設定檔被改了也還是對的）。
  代理仍能通，是因為 `web/vite.config.ts` 把 `Origin` 改寫成 daemon 自己的位址（daemon 的 Origin 檢查照舊只信 localhost）。
  代價要知道：同網段任何裝置都能透過 5173 的 `/api` 代理打到 7788，公共網路上要另外收斂。
- **看門狗** launchd `com.agm.dev-server`（`~/Library/LaunchAgents/com.agm.dev-server.plist`）：
  `StartInterval 300`、`RunAtLoad true`，`ProgramArguments` 是
  `/opt/homebrew/bin/bun run supervisor/AGM/bin/dev-server-kick.ts`，`EnvironmentVariables.PATH` 含
  `/opt/homebrew/bin`（bun）與 `/Users/m4p/.local/bin`（node）——launchd 不給登入 shell 的 PATH，
  少了這行就會「手動跑得起來、排程跑不起來」。看門狗自己用 bun 沒問題：它只做 fetch / lsof / spawn，
  不當 HTTP 代理，碰不到上面那個 socket 差異。腳本的行為順序：
  1. **健康 = 對外可達**，不是「127.0.0.1 有回應」：先用 `lsof -nP -iTCP:5173 -sTCP:LISTEN -Fpn` 看
     LISTEN 的位址，綁 `*:5173`／`0.0.0.0:5173` 且 curl `127.0.0.1` 有回應才算健康 → 直接 `exit 0`，
     **不寫 log**（每 5 分鐘一行會把 log 灌爆）。只綁 `127.0.0.1`／`[::1]` 的實例本機看得到、使用者的手機
     看不到，一律當成**錯誤實例**。（`lsof` 要用 `-F` 機器格式：人類格式的最後一欄是 `(LISTEN)` 不是位址，
     照欄位切會把每顆都誤判成 loopback-only。）
  2. 錯誤實例怎麼處理，看它是誰的：
     - **vite 且 `ppid=1`**（孤兒，起它的人已經結束）→ kill 掉，寫 `收掉孤兒 loopback-only vite pid N`，
       等 port 放開（最多 5 秒，沒放開就交下一輪），再照下面拉起一顆綁 `0.0.0.0` 的。
     - **vite 但還有活著的父程序** → 某個 bot 正在用，只寫 `…以 loopback-only 占用，需人工處理`，不 kill。
     - **非 vite 程序** → 一律只記錄 pid 與完整 command，不 kill。
  3. 找不到 node 或找不到 `vite.js` → 寫 log 跳過這輪，**不拿 bun 代跑**（等於把 crash 裝回去）。
  4. 真的沒人聽 → `nohup node vite.js --host 0.0.0.0 --port 5173 --strictPort`，最多等 15 秒複驗；
     仍失敗就寫 log 交給下一輪，**不在腳本裡重試迴圈**（web/ 編不過時才不會每 5 分鐘炸一次）。
  log 在 `supervisor/AGM/dev-server.log`；launchd 自己的 stdout 在 `dev-server.launchd.log`。
  舊的 bash 版留成 `dev-server-kick.sh.bak-bun`，確認 .ts 版跑滿一輪沒問題後刪掉。
- 這條規則的由來：2026-09-12 有 bot 用 `npx vite --port 5173 --strictPort`（沒帶 `--host`）起了一顆只聽
  `[::1]` 的實例，看門狗當時只認「127.0.0.1 有回應」，於是判定「被占用、不動它」，5173 對使用者等於掛著。
  現在兩道都補上了：`vite.config.ts` 的 `server.host: true` 讓手動起的也對外，看門狗則會收掉沒人認領的
  loopback-only 孤兒。
- **其他 port 不歸看門狗管**：5188 那類 `VITE_MOCK=1` 實例是各 bot 自己的測試環境，AGM 不碰、不清、不重啟。
- **驗證方式**（改動這條規則或腳本後要重跑）：kill 掉現有 vite → 跑一次 kick.sh → `curl http://127.0.0.1:5173/`、
  `curl http://<LAN IP>:5173/`、`curl http://<LAN IP>:5173/api/session` 都要 200（最後一項才證明代理活著）。
  LAN IP 用 `route -n get default` 找到的那張介面問，**不要寫死 `en0`**（這台的 LAN 是 `en7`）。

### 18.2 正式 daemon 的定義與例行更新

- **正式 daemon** = `target/release/agents-managerd serve`，監聽 `127.0.0.1:7788`，前端（`web/dist`）內嵌在這個
  release binary 裡。使用者口語的「7788」就是它；**5173 是開發用 vite，不是正式環境**。前端改動要
  `bun run build` **再** `cargo build --release -p agents-managerd` 才會進到 7788。
- **例行檢查** launchd `com.agm.daemon-update`：`StartCalendarInterval {Minute: 0}`（**每小時整點**），
  跑 `supervisor/AGM/bin/daemon-update-kick.sh`。腳本順序：`git fetch` → 沒有程式碼差異就跳過（下一條）→
  同一 commit 已派過就 skip（`daemon-update.last`）→ 建置 child 不存在就寫 `build child missing` 並
  `exit 0`（**不改派給別人**）→ **上一筆更新派工還沒結案就 skip**（`client_request_id` 以
  `agm-daemon-update-` 開頭且 status 不是 `completed`／`failed`，寫 `previous update still pending (<id>)`；
  否則每個整點會疊派同一件事，送達時才發現早就做完了）→ **還有 bot 在 working 就 defer** → 派
  `daemon-update-task.md`，`--request-id agm-daemon-update-<sha>`（同版不重派）。
- **「可以動手」的判準是沒有 bot 在 `working`，不是 `busy ≤ 1`**：`bin/agm health` 的 `bots.busy` 把
  `blocked` 也算進去，而 blocked 是**在等使用者回答**——可能好幾小時，重啟卻不會打斷它（pane 不動，
  原 pane 重啟本來就跳過 blocked）。照 busy 等的話，只要有一顆 bot 在等人，例行更新就永遠派不出去。
  所以 kick.sh 與固定條件 3 都改看 `bin/agm --compact state` 的 `run.agent_status == 'working'`
  （排除建置 child 自己與 AGM）。daemon 的 `health.busy` 語意不動，UI 還在用它。
- **誰做重建**：(a) 有 bot 自己申請重建（帶已 push 的 commit）→ 核准後**由申請的 bot 自己建**；
  (b) 沒有申請者的例行更新 → 固定由 **AGM 建置 child `agm-pxf2pv-build`（cc0/opus/low）**。
  **絕不派給使用者的專案 bot**——會白耗它們的 context。
- **docs-only 的差異不重啟**，防線在 kick.sh：每次建置成功並驗證通過後，建置者把這次建進 binary 的
  origin/main short sha 寫進 `supervisor/AGM/daemon-update.built`（回滾就不寫）。kick.sh 有這個檔就改用
  `git diff --quiet <built> origin/main -- daemon web Cargo.toml Cargo.lock` 判斷：沒有差異就寫
  `docs-only since <built>, skip`、把 `daemon-update.last` 推到 HEAD（下個整點不必重算）並結束；
  檔案不存在或裡面的 sha 不在 repo 裡，才退回舊的「binary mtime vs origin/main commit 時間」比較
  （那個比法會把 docs-only 也算成落後，是這條規則的由來）。
  執行端保留同一道檢查：接到任務先自己 diff 一次，沒有程式碼差異就只做驗證並回報「無需上線」，
  **不要為了零程式碼差異中斷使用者與所有 bot**。
- **重建重啟的固定條件**（六項全中才動手，否則回報阻塞）：
  1. 在**乾淨的 HEAD worktree** 建 web 與 daemon（用 `git worktree`，共用樹裡永遠有別人未提交的 WIP，
     不得把它編進 release，也不得 stash / reset）。
  2. 整樹 `cargo test -p agents-managerd` 全過，web `bunx tsc --noEmit -p tsconfig.app.json` 通過
     （`tsc --noEmit` 不帶 `-p` 是假綠燈）。
  3. 等到沒有別的 bot 在 `working`（`blocked` 不算，見上一條）；有人在跑就等，最多 30 分鐘，
     超過回報「延後」不硬重啟。**判定通過之後、換 binary 的前一刻要再查一次**，仍成立才動手——
     建置與等待之間隔了好幾分鐘，狀態會翻回來（2026-09-12：輪詢判定「只剩一顆授權中的 bot」，
     幾秒後另一顆又變成 working，腳本沒有再閘一次就換了 binary）。
     「再查一次」本身也還是快照：查完到動手之間仍有空隙。§18.10 的執行租約就是為了把這段補起來——
     `acquire` 在同一個鎖裡重驗並**持有**窗口，期間 daemon 不再派新工作，所以條件在整段執行期間持續成立。
  4. 備份舊 binary 為 `target/release/agents-managerd.bak`。
  5. 重啟後 **30 秒內**驗 `/api/session` 與 `bin/agm health`。
  6. **60 秒內**確認 `bin/agm supervisor` 的 status 不是 stopped、running 名單沒少、沒有 bot 被無故關 pane。
     任一項不對就用 `.bak` 回滾並回報。
  期間不要同時觸發「claude 更新重啟」去動其他 bot（已知競態：2026-09-10 23:02Z 把 AGM 等 4 顆 bot 殺掉沒拉回，
  已由 §6 的批次重啟修正處理）。

### 18.3 喚醒 AGM 的節流

`[supervisor] notify_interval_secs`（預設 600 秒）——規則與理由見 §5「設定檔」。重點：事件照舊即時寫入
`supervisor_inbox`，被節流的只有「推給 AGM、喚醒它」這個動作；health 偵測（30 秒）、watchdog 與模型控制器
TICK 都不受影響。API 端的狀態機見 `docs/API.md` 的 `GET /api/supervisor/inbox`。

### 18.4 瀏覽器殭屍清理

launchd `com.agm.browser-gc` 跑 `bin/browser-gc-kick.sh`，`StartInterval` 依使用者的 Claude 訂閱方案定
（2026-09-12 使用者裁示；方案變了就改 plist 與這張表）：

| 方案 | StartInterval | 頻率 |
| --- | --- | --- |
| Pro | 21600 | 每 6 小時 |
| Max 5x | 3600 | 每 1 小時 |
| Max 20x | 1800 | 每 30 分鐘（**目前**） |

每輪：確認
`agm-pxf2pv-browser-gc` 在跑（沒跑就 `bin/agm bot start`），再把 `browser-gc-task.md` 派給它，
request id 綁時間（`agm-browser-gc-<YYYYmmdd-HHMM>`）。清理規則：

- ego lite：`listTaskSpaces()`，`ownership=agent` 且沒有進行中的 assignment／最近 2 小時無活動才
  `completeTaskSpace(id, {keep:false})`；**`ownership=user` 或 `agentDelegatedToUser` 一律不動**。
- Chrome：只動 Claude in Chrome 的 MCP tab group，其餘使用者分頁不碰。
- **bot 的 headless Chrome（CDP 截圖用）**：`--headless=new --remote-debugging-port=93xx
  --user-data-dir=/tmp/am-cdp-*｜/tmp/am-codex-*-profile｜/tmp/am-ui-rc` 這一類。使用者自己的 Chrome 沒有
  `--headless`，永遠不在清單裡。判定（前兩項 `kick.sh` 用 shell 直接做，不花 LLM）：
  - **孤兒（`ppid=1`）+ debug port 上沒有 ESTABLISHED 連線 + 活超過 2 分鐘** → TERM，等 3 秒再 KILL，
    `/tmp/am-*` 的 profile 目錄一併 `rm -rf`。三個條件缺一不可：bot 用 `nohup` 起的實例在 bot 還活著時
    `ppid` 也是 1，只看孤兒會殺掉正在截圖的實例；正在用的實例一定有一條 CDP 連線。
  - **沒有 Chrome 在用、一小時內沒被動過的 profile 目錄** → 刪。一個目錄 100～230 MB，放著就是好幾 GB
    （2026-09-12 清出 ~1.5 GB，另有 ~1.2 GB 因為剛被動過留到下一輪）。
  - **父程序活著、但那顆 bot 已經 `idle`／run 結束，且 Chrome 活超過 30 分鐘** → 做完截圖沒關，收掉並記是誰。
  - **父程序活著且 bot 是 `working`／`blocked`** → 保留，列出 bot 與 profile 路徑。
  回報固定一行：`headless Chrome：收掉 N（profile）／保留 M（bot）`。
- **所有 bot 的義務**：用 CDP／headless Chrome 截圖，**用完自己關**（`Browser.close`，或 kill 自己 spawn 的
  pid），profile 目錄用完即刪。browser-gc 是安全網，不是代收垃圾的——2026-09-12 就累積到 10 顆實例、
  最久的從 9/9 活到 9/12，加上沒刪的 profile 共約 5 GB。
  `ps` 的 etime 在 macOS 沒有 `etimes` 可用，要自己把 `[[D-]HH:]MM:SS` 換算成秒。
- CLI 超過 30 秒無回應：改用 `ps` 列出 renderer 的 pid／記憶體／存活時間回報，**不要直接 kill ego lite 主程序**；
  只有確認 CLI 無回應時，才走「quit → `pkill -f '/Applications/ego lite'` → 對殘留的
  `--startup-ego-browser-service` `kill -9` → `open -a`」這條會關掉所有視窗的路。
- 記憶體不足導致指令被殺就回報並停止，不要重試迴圈。

### 18.5 AGM 派 child 的模型預設

`cc0/opus/low`。不預設 `fable`，也不預設 `high` 以上的強度（`high`／`xhigh`／`max`）——對一般修正與覆核工作
過度，只有任務明確需要才調高並記錄理由。AGM **自己**的模型不在此列，由 supervisor 控制器依 §5 的候選規則切換
（`fable` 剩餘 <5% 切 `opus`，30 分鐘冷卻內只自動切一次，不會自動切回）。

### 18.6 persona 的權威與四份同步

> **2026-09-12 起由 §18.11 取代。** 「重跑 setup 會蓋回舊版」已經修掉：持久版才是權威，setup 只在完全沒有
> 人設時 seed。改人設走 `PUT /api/supervisor/persona`（它自己會同步下面那幾份副本）。這一節保留為當時的
> 狀況紀錄與「不要手改 config.toml」那條仍然有效的規則。

AGM 的 persona 有四份副本，改動時**四份一起改、逐字一致**，否則重跑 supervisor setup 會把現行 persona 蓋回舊版（#62）：

| 副本 | 位置 | 怎麼改 |
| --- | --- | --- |
| 來源 | `docs/goals/agm-supervisor-persona.md`（`---` 以下） | commit + push；`supervisor/setup.rs` 的 `include_str!` 內嵌的就是它 |
| 設定檔 | `~/.config/agents-manager/config.toml` 的 AGM bot | 由 `PATCH /api/bots/{AGM}` 連帶寫入 |
| 資料庫 | `bots.persona` | `PATCH /api/bots/{AGM}`（回 `needs_restart: true`） |
| 執行期 | `~/.config/agents-manager/supervisor/AGM/persona.md` | 直接寫檔 |

- **手改 config.toml 之後走 API 是可以的，但別指望它保住手改的內容**：`ConfigStore` 每次寫入前用 mtime 比對，
  發現磁碟版本變了就在同一把 mutex 內**先重讀磁碟版本再套用**這次更新（§5；`config.rs::update`，`issue28_tests` 驗證），
  只有重新解析失敗才回錯（`config.toml changed on disk and could not be re-read`）。所以手改沒有壞語法時 API 照常成功，
  而且手改的內容會被保留；但 serde 全量回寫會把註解與未知欄位洗掉。要改 persona 還是走 API，讓 daemon 自己寫檔。
  （本段原本寫「會一路 409 直到 daemon 重啟」，與實作不符——2026-09-12 review #10 改正。）
- persona 改完**不必**為它重啟 daemon：`needs_restart` 只表示下次 AGM 重啟才載入新 persona。
  反過來也成立，而且比較容易搞錯：`needs_restart=false` **不等於**新人設已經在 session 裡生效（§18.11）。

### 18.7 共用工作樹規範

`/Users/m4p/project/agents-manager` 是多個 bot 共用的同一個工作樹，任何時候都可能有別人未提交的改動：

- **不改別人的 WIP，連「只是新增幾行」也不行**；不 `git stash`、不 `reset`、不 `checkout -- <file>`、
  不 `--autostash`，也不要用 regex 對共用樹批次取代（會掃到別人正在改的檔）。
- 認定某份 WIP 的擁有者要有**證據**（那個 bot 自己的訊息／assignment），不能憑 pane 標題或 session 名稱猜。
- 工作樹髒到不能 `pull --rebase` 時，用暫存 index（`GIT_INDEX_FILE` + `read-tree origin/main` +
  `commit-tree`）把自己的檔案接在 `origin/main` 上直接 push，或另開 `git worktree`；本地 HEAD 落後沒關係。
  腳本要用 `/usr/bin/git` 並寫成 bash 檔（zsh 不拆 `$VAR`，rtk 會吞掉失敗，曾因此推出空 commit）。
- 需要重疊範圍時交回 AGM 分配 ownership，不要自行合併或替別人收尾。


### 18.8 交辦的執行與驗收是兩件事（2026-09-12）

回合結束只結束**那一輪**。2026-09-12 的 review 找到實例：assignment `01M246903Z54XW872GWD7XXJAE` 的回覆是
「還在等編譯（已等 78 分鐘），稍後回報」，狀態卻已經是 `completed`，而未結案查詢立刻把它排除掉——沒做完的工作
就這樣從待辦裡消失。

所以 `supervisor_assignments.status` 改成生命週期：

| 階段 | 狀態 | 誰能推動 |
| --- | --- | --- |
| 執行中 | `queued` / `delivered` / `unknown` | daemon（派送、退避、對帳） |
| 等驗收 | `awaiting_review` | 只有 AGM 的決定 |
| 已決定 | `completed` / `failed` / `cancelled` / `superseded` | AGM |
| 還在等 | `blocked` | AGM，且**仍算未結案** |

- 回合的原始事實不丟：`delivery`、`turn_status`（`completed` / `completed_fallback` / `failed` / `dispatch_failed` /
  `turn_missing`）與 `evidence_complete` 各自留欄位。終端備援（`completed_fallback`）不會因為「跑完了」就被驗收。
- 連派不出去的交辦也是進 `awaiting_review`（`turn_status=dispatch_failed`）。daemon 知道送失敗，不知道這份工作該
  怎麼辦；讓它自己結案就是在替使用者以為還在跑的工作蓋章。
- 驗收走 `POST /api/supervisor/assignments/{id}/review`，每次記 actor、來源、理由與證據（`supervisor_reviews`）。
  同樣的 decision 重送是冪等的。回合還在跑時只接受 `cancel`（而且不會中止那個回合，之後的回覆不會再記到這筆
  交辦上）；`block` 要等回合結束停在 `awaiting_review` 之後再標，不然 row 會在回合還開著時離開執行中集合。
- 「要求續作」是**新開一筆** `follow_up_of` 指回原本那筆的交辦（原本那筆變 `superseded`），用呼叫端給的穩定
  `followup_request_id` 去重——不改寫已經送出去的文字，bot 不會憑空看到自己沒收過的指示。
- 未結案 = `queued`/`delivered`/`unknown`/`awaiting_review`/`blocked`。open count、handoff、`/supervisor/state`、
  UI 與例行更新判斷都吃這一組。
- **既有資料**：migration 只把已經關掉的舊 row 標成 `legacy_closed=1`，不重新打開、不重新派工；UI 與 API 標示
  「舊資料·未經驗收」，不宣稱它們被驗收過。

### 18.9 總管健康與系統 incident（2026-09-12）

`manager_health`（AGM 自己能不能工作）與 `system_health`（系統有沒有壞）分開；頂層 `status` 是兩者取較嚴重者的
相容投影，所以只讀 `status` 的舊呼叫端不會在 host 掛掉時看到 `healthy`。

incident 以**資源**為單位持久化（`supervisor_incidents`，`(kind, resource)` 在 `status='open'` 上唯一）：

| kind | 判斷 | 門檻（`[supervisor]`） |
| --- | --- | --- |
| `host_disconnected` | host 連不上 | `host_disconnected_secs`（120） |
| `bot_stopped` | `autostart=1` 的 bot 沒有 active run | `bot_stopped_secs`（300） |
| `assignment_stalled` | 未結案交辦 `updated_at` 沒動 | `assignment_stalled_secs`（7200） |
| `assignment_undelivered` | 還在 `queued`、從沒送出去，用 `created_at` 算 | `assignment_stalled_secs`（7200） |
| `notify_exhausted` | 通知重送用盡預算 | `notify_max_attempts`（5） |

- 條件要**持續**超過門檻才寫入（門檻計時在記憶體裡，重啟後重算——寧可晚一點開，不要重複開）；開啟與恢復各推
  一則 inbox 事件，中間的每一 tick 只更新 `occurrences`。恢復後再壞是新的一筆，不是舊的復用。
- `assignment_undelivered` 要獨立一條，是因為 `assignment_stalled` 天生看不到它：每次重試都呼叫 `defer`，
  `updated_at` 就被推到現在，於是「對著停掉的 bot 每 5 分鐘重試一次」看起來永遠很忙。派工被拒的真正理由
  （`bot has no active run`、`needs_login` 等）記在 `error` 欄，不是分類字串 `conflict`（2026-09-13 review #30）。
- 不算故障：使用者自己停掉的 bot、正常等使用者回答的 blocked pane、短暫排隊、AGM 自己的 idle/busy 變換。
  量不到的東西回 `unknown`，不併進 `healthy`。
- 全部走既有的 30 秒 cheap probe，不因為要判斷而額外問模型或殺程序。


### 18.10 重建／重啟的核准與執行租約（2026-09-12）

原本「AGM 說可以」只存在於對話裡，安全檢查是一次快照：`daemon-update-kick.sh` 讀一次「有沒有人在 working」，
然後花好幾分鐘建置與替換 binary，期間隨時可能有人開始工作；兩個申請也可能同時被允許「等空檔執行」。

現在窗口分成兩個明確的階段：

1. **等安全窗口**：`GET /api/supervisor/maintenance/safety`，唯讀，只描述「現在」。
2. **取得排他窗口**：`POST /api/supervisor/leases/{resource}/acquire`，在同一個 supervisor lock 裡重驗核准與
   idle，再以單一條件式 UPDATE 拿走租約。搶同一個窗口只有一個會成功。

- 核准是紀錄不是句子：申請者、purpose、範圍、`target_commit`、有效期、誰決定的、理由。acquire 時逐項核對，
  purpose 不符、過期、被撤銷、commit 不同都會被拒。release 時把核准標成 `consumed`——一次核准一個窗口。
- 租約有 `fence`，只增不減。持有人過期後被別人接手，舊 fence 的 renew／release 一律失敗，所以「昨天核准過」
  不會變成現在還能動手。租約到期即自動釋放，crash 不會永久鎖死。
- 拿著 `restart` 租約期間，assignment 派送會 hold（留在 `queued`，不丟工作也不算重試次數）——這正是快照做不到的
  那一半：不會一邊確認空閒、一邊又派新工作進去。
- assignment 可帶 `ownership`（檔案／模組）。重疊時 `POST /assignments` 回 `ownership_conflicts`，**只回報不阻擋**：
  daemon 無法判斷兩個模組是不是真的獨立，這是交給 AGM 協調的資料，不是鎖。
- 運維腳本進 repo（`scripts/ops/`），改用上面的流程，並附隔離測試（`scripts/ops/daemon-update-kick_test.sh`，
  假 CLI ＋ 暫存 repo，不碰正式環境）。正式安裝由 AGM 決定時機。
- **邊界（明講）**：租約只約束走 API 與這些腳本的路徑。任何一個 shell 仍可直接 kill daemon 或自己跑
  `cargo build --release`，daemon 這裡沒有 OS 層的鎖可以強制。租約讓「問過 AGM」在執行期間持續成立，不是取代它。

### 18.11 人設的權威、版本與建置依賴（2026-09-12）

人設原本有四份（repo、config.toml、DB、總管 cwd 的可讀副本），review 又點出兩份沒被算進去的：
**binary 內嵌版**與 **session 已載入版**。而 `ensure_env` 會無條件把內嵌版寫回 bot——舊 binary 跑一次 setup
就可能把剛更新的人設降回它自己編進去的那版。

- **持久版（`supervisors.persona_text`）是權威。** `setup` 只在完全沒有人設時 seed 一次；之後 seed 一律被拒。
  `persona_seed_hash` 記住當初 seed 自哪個內嵌版，所以「內嵌版有新的」與「這份是被刻意改過的」分得開。
- 內嵌版要取代持久版只有一條路：`POST /api/supervisor/persona/adopt-embedded`，明確的遷移，有 actor 與理由。
- `config.toml` 的 bot persona 與 `persona.md` 都是**從持久版產生的副本**，改人設走 `PUT /api/supervisor/persona`
  （帶 `expected_version` 可做樂觀鎖），不要手改檔案。
- **已載入版不可觀測，就不要假裝。** daemon 只能在啟動 CLI 時把 persona 傳進去，看不到 session 現在握著什麼
  （compaction、`/clear` 在外面都看不見）。所以 `loaded.status` 只有 `unknown`／`stale`／`unverified`，沒有
  `verified`；`needs_restart=false` 只代表「不是舊 session」，不代表全文已載入。
- **建置依賴**：`GET /api/supervisor/build-inputs` 列出會進 binary 的路徑，含 `include_str!` 的
  `docs/goals/agm-supervisor-persona.md` 與 `scripts/agm.py`。一般 docs 改動不必重建；這些改了就是 binary 落後，
  但**什麼時候重建、什麼時候重啟仍由 AGM 決定**——這兩件事分開判斷。清單與實際 `include_str!` 由測試綁住
  （`supervisor::persona::tests::every_embedded_file_is_declared_as_a_build_input`），不靠人記得改。

### 18.12 遠端（手機）入口的可觀測性（2026-09-12）

AGM 是使用者唯一的手機入口，但 daemon 一直無法回答「這個入口現在通不通」：`setup` 把
`--remote-control AGM` 寫進 args，那是**要求**，而沒有任何一條路徑去驗證 session 有沒有起來。

先查能力，再決定怎麼講：herdr pane 狀態、hook payload、session 資料列、bot args 都不帶 Remote Control 的
session 資訊；整個 repo 裡 `--remote-control` 只出現在 setup 寫進去的那個參數。**結論是目前沒有可靠的觀測來源**，
所以 `capability.status = unsupported`，daemon 不會宣稱手機已連上。

- 狀態只有 `requested`（argv 要求過）、`verified`（有可驗證來源說通了）、`unavailable`（有證據說不通）、
  `unknown`。**沒有 `active`。** 從 argv、從 bot 自己的文字、或從一個 URL 都不能推出 `verified`。
- 觀測綁 session（run id）並且會過期（900 秒）。AGM 重啟或換模型後是新的 session，舊的確認一律失效
  （`revoked: session_changed`）；過期是 `observation_expired`。撤銷時 `url` 也不再回傳。
- 需要人工確認時走 `POST /api/supervisor/remote`，`source=manual` 且**必須帶 actor**——它被記成「某個人在某個
  時間宣稱過」，不是 daemon 的量測，UI 也照這樣寫。
- incident 只在 `unavailable`（有證據的失敗）時開。`unknown` 配 unsupported 是已知限制，不是故障；為它開
  incident 只會得到一盞永遠關不掉的紅燈。
- 恢復（例如重開 remote session）仍須 AGM 在沒有回合衝突的窗口安排，daemon 不會自己多開 session，也不動其他
  使用者入口。

### 18.13 這批改動的 migration 與回滾限制（2026-09-12）

全部是 additive column 與新資料表（`supervisor_reviews`、`supervisor_incidents`、`supervisor_approvals`、
`supervisor_leases`），`db::migrate` 重跑冪等，升級不必停機做資料搬移。要注意的是**往回滾**：

- 舊 binary 看不懂 `awaiting_review` / `blocked` / `superseded`。它的 open 查詢只收
  `queued`/`delivered`/`unknown`，所以這些交辦會從它的未結案清單**消失**（資料還在，不會被刪）。
  真的要回滾，先把當下的 `awaiting_review` 逐筆決定掉，別留在半途。
- 舊 binary 的 `on_turn_done` 會把回合跑完的交辦直接寫成 `completed`——也就是回到這次要修的行為。
  回滾後新做完的那些交辦不會有 `legacy_closed` 標記，之後再升級上來也分不出來。
- `legacy_closed=1` 的回填只跑一次（欄位建立時）。升級後再回滾再升級，中間那段用舊語意關掉的 row
  不會被重新標記。
- 租約與核准對舊 binary 無效：它不看 `supervisor_leases`，所以回滾期間 restart 窗口不會 hold 住派工。
- persona：新版把持久版當權威，舊版的 `ensure_env` 仍會用內嵌版覆寫。回滾前先確認內嵌版就是你要的那份，
  否則舊 binary 跑一次 setup 就把自訂人設蓋掉（§18.11 修的就是這個）。

## 附錄 A：herdr socket 實測結果（2026-09-05，herdr 0.8.2 / protocol 20）

- 線路格式：每個請求一條 JSON line `{"id":"<string>","method":"...","params":{...}}`，`id` **必須是字串**；回應 `{"id","result":{"type":...}}` 或 `{"id","error":{"code","message"}}`。錯誤碼例：`agent_not_found`、`workspace_not_found`、`pane_not_found`、`agent_not_ready`、`agent_blocked`、`invalid_request`。
- **一個連線只處理一個請求**，回應後伺服器即關閉連線；`events.subscribe` 例外，先回 `{"result":{"type":"subscription_started"}}` 後持續推送事件行。
- 訂閱 `pane.agent_status_changed` **必須帶 pane_id**；一次 subscribe 可帶多個 filter。實測：對 pane 送 prompt 後依序收到 `working`、`idle`，事件名為 `pane.agent_status_changed`（點號），data 含 `pane_id`、`workspace_id`、`agent_status`、`agent`。
- 全域 `pane.updated`（事件名 `pane_updated`，底線）**不會**穩定反映 agent 狀態轉換，只在訂閱初期收到一批快照；不可用作狀態來源。
- `agent.start` 透過 socket 是**非同步**的，立即回傳 `launch_pending:true`；需 `agent.wait {until:[idle,done,blocked]}`。實測 claude 約 10 秒、codex 約 4 秒就緒。
- `agent.read` 的 `source`：`visible | recent | recent_unwrapped | detection`。
- `agent.prompt` 帶 `wait:{timeout_ms}` 可同步等；實測 claude 回覆約 6 秒、codex 約 3 秒。
- `session.snapshot` 回傳 `{version, protocol, workspaces[], tabs[], panes[]（含 agent、agent_status）}`；agent name 需另以 `agent.list` 取得。
- named session：`herdr --session <name> server`；socket 在 `~/.config/herdr/sessions/<name>/herdr.sock`；CLI 以 `herdr --session <name> ...`。
- daemon 從 Claude Code 內啟動時 pane 會繼承 `CLAUDE_CODE_CHILD_SESSION`，導致 claude 不保存 transcript；`env` 可覆寫為空字串（效果需以 hook 的 `transcript_path` 驗證）。

## 附錄 B：agent hook 實測（2026-09-05）

- Claude Code v2.1.261：`claude --settings <絕對路徑>` 可注入 hooks；`Stop` stdin 含 `session_id`、`transcript_path`、`prompt_id`、`last_assistant_message`、`stop_hook_active`。**Stop hook 的 stdout 若為 JSON 會被解讀為決策，子命令必須空 stdout。**
- codex-cli 0.153.4：`codex -c 'notify=["<abs>", ...]'` 可覆寫 notify；argv 最後一項為 JSON，含 `type: agent-turn-complete`、`thread-id`、`turn-id`、`cwd`、`input-messages`、`last-assistant-message`。

## 附錄 C：SQLite schema

```sql
PRAGMA foreign_keys = ON;   -- 每個連線
-- 時間欄一律 RFC3339 UTC 字串
CREATE TABLE projects (
  id TEXT PRIMARY KEY, path TEXT NOT NULL UNIQUE, label TEXT NOT NULL,
  workspace_id TEXT,                          -- NULL = 尚未建立或已消失
  deleted_at TEXT, created_at TEXT NOT NULL
);
CREATE TABLE bots (
  id TEXT PRIMARY KEY, project_id TEXT NOT NULL REFERENCES projects(id),
  name TEXT NOT NULL, kind TEXT NOT NULL CHECK (kind IN ('claude','codex','grok')),  -- v3.5 加 grok
  args_json TEXT NOT NULL DEFAULT '[]', autostart INTEGER NOT NULL DEFAULT 0,
  hook_token TEXT NOT NULL, deleted_at TEXT, created_at TEXT NOT NULL
);
CREATE UNIQUE INDEX bots_name_live ON bots(name) WHERE deleted_at IS NULL;
CREATE TABLE runs (
  id TEXT PRIMARY KEY, bot_id TEXT NOT NULL REFERENCES bots(id),
  state TEXT NOT NULL CHECK (state IN ('starting','running','stopping','stopped','exited')),
  agent_status TEXT NOT NULL DEFAULT 'unknown' CHECK (agent_status IN ('idle','working','blocked','unknown')),
  workspace_id TEXT, pane_id TEXT, adopted INTEGER NOT NULL DEFAULT 0,
  native_session_id TEXT, transcript_path TEXT,
  last_read_revision INTEGER, last_read_tail_hash TEXT,
  started_at TEXT NOT NULL, ended_at TEXT
);
CREATE UNIQUE INDEX runs_one_active ON runs(bot_id) WHERE state IN ('starting','running','stopping');
CREATE INDEX runs_pane ON runs(pane_id);
CREATE TABLE conversations (
  id TEXT PRIMARY KEY, bot_id TEXT NOT NULL UNIQUE REFERENCES bots(id), created_at TEXT NOT NULL
);
CREATE TABLE turns (
  id TEXT PRIMARY KEY, conversation_id TEXT NOT NULL REFERENCES conversations(id),
  run_id TEXT REFERENCES runs(id),
  origin TEXT NOT NULL CHECK (origin IN ('web','external')),
  status TEXT NOT NULL CHECK (status IN ('in_flight','completed','completed_fallback','failed')),
  delivery TEXT NOT NULL DEFAULT 'pending' CHECK (delivery IN ('pending','ok','unknown','failed')),
  client_request_id TEXT,                     -- NULL for external；應用層禁止空字串
  native_session_id TEXT, native_turn_id TEXT, -- NULL 直到 hook 回填
  created_at TEXT NOT NULL, completed_at TEXT
);
CREATE UNIQUE INDEX turns_one_in_flight ON turns(run_id) WHERE status = 'in_flight';
CREATE UNIQUE INDEX turns_client_req ON turns(conversation_id, client_request_id) WHERE client_request_id IS NOT NULL;
CREATE UNIQUE INDEX turns_native ON turns(native_session_id, native_turn_id) WHERE native_turn_id IS NOT NULL;
CREATE INDEX turns_conv_time ON turns(conversation_id, created_at);
CREATE TABLE messages (
  id TEXT PRIMARY KEY, conversation_id TEXT NOT NULL REFERENCES conversations(id),
  turn_id TEXT REFERENCES turns(id),
  role TEXT NOT NULL CHECK (role IN ('user','assistant','system')),
  content TEXT NOT NULL,
  source TEXT NOT NULL CHECK (source IN ('web','hook','transcript','terminal_fallback','system')),
  incomplete INTEGER NOT NULL DEFAULT 0, terminal_snapshot TEXT,
  group_id TEXT,                              -- §13：同一次群組發言的副本 / 註記共用；其他為 NULL
  created_at TEXT NOT NULL, updated_at TEXT
);
CREATE INDEX messages_conv_time ON messages(conversation_id, created_at);
CREATE INDEX messages_turn ON messages(turn_id);
CREATE INDEX messages_group ON messages(group_id) WHERE group_id IS NOT NULL;
```

## 附錄 D：實作里程碑

| # | 里程碑 | 驗收 |
|---|---|---|
| M1 | herdr client + session 管理 | `agents-managerd serve` 拉起 named session；log 出現 ping protocol 20；手動在該 session 送 prompt 給一個 agent，daemon log 出現 `pane.agent_status_changed` |
| M2 | config 載入 / 補 id / 寫回、SQLite migrations、TOML→SQLite 投影、`GET /api/state` | 手寫 config 含 1 project 2 bots → `curl` 回傳，id 已補寫 |
| M3a | 單 Bot start / stop | `POST start` 後 `herdr --session agents-manager agent list` 看到；`stop` 後 pane 消失；並行兩次 `start` 其中一次 409 |
| M3b | 對帳 | kill daemon（不停 herdr）→ 重啟 → 同一 pane_id、不出現第二個 agent；手動關 pane → Run `exited` |
| M4 | hook 子命令 + `/hook/*` + prompt → assistant（source=hook） | 真 Claude / Codex 各一次 `Reply PONG`；**假 hook 時序測試**（`curl` 直接打 `/hook/*`）：hook 早於 prompt RPC 回應、重複 `prompt_id`、SessionStart 不建 Turn、daemon 停機時子命令 3 秒內 exit 0 且寫 spool、重啟後補入 |
| M5 | blocked + keys + 終端備援 | 對 codex 下需確認的指令 → 狀態 `blocked`、`GET terminal` 看到提示、`POST keys ["y"]` 恢復；停用 hook 注入後 prompt → 5 秒後出現 `terminal_fallback` 訊息 |
| M6 | WebSocket | 單客戶端收到 `bot_status`、`message_added`；斷線重連收到 `resync` 後重新載入 |
| M7 | React UI | 瀏覽器完成：新增 Project 與 Bot → start → 對話 → blocked 處理 → stop |
| M8 | 前端內嵌、`cargo run --release -- serve` 一鍵啟動 | 開 `http://127.0.0.1:7788` 可用 |

風險最高：M4（hook 配對）與 M3b（對帳）。M5 的 blocked 依賴 herdr 辨識，若不穩可先以假狀態驗 UI。

## 附錄 E：遠端 herdr 實測（2026-09-06，本機 → m4p@100.112.229.82，macOS / herdr 0.8.2）

- 非互動 ssh shell 的 PATH 只有 `/usr/bin:/bin:/usr/sbin:/sbin`；herdr 在 `/opt/homebrew/bin`，claude / codex 在 `~/.local/bin`。pane 內的 shell 是互動 shell，PATH 正常。
- `herdr --session agents-manager server` 可用 `nohup … &` 從 ssh 啟動並存活。
- `ssh -N -M -S <ctl> -L <local.sock>:<remote herdr.sock> …` 轉發後，以本機 `HerdrClient` 直接 `ping` 成功（約 137 ms），`workspace.create`、`agent.start`、`agent.wait`、`agent.send_keys`、`agent.prompt`、`events.subscribe`（逐 pane `pane.agent_status_changed`）全部正常。AF_UNIX 路徑過長會 `path too long`，需用 `/tmp` 短路徑。
- **Keychain**（v3.4 實測）：`ssh m4p 'security find-generic-password -s "Claude Code-credentials" -w'` 失敗（36）；同一指令放進 `launchctl bootstrap gui/501` 的 LaunchAgent 執行成功。m4p 預設 `~/.claude` 有 `.credentials.json` 所以不受影響，`~/.claude-ccompany` 只有 Keychain 憑證，在 ssh 起的 herdr 底下就「Not logged in」。改為 launchd 後 cc1 身份的 bot 4 秒內回覆（hook）。`herdr --remote` 也是經 ssh 拉起遠端 server（binary 內無 launchctl），同樣受限，且它只是 TUI 串流、無 socket API 轉發。
- 遠端 claude 首次啟動出現「trust this folder」提示 → herdr 回報 `blocked`；游標預設在「No, exit」，需送 `down` + `enter`。
- `ssh -O forward -R 17788:127.0.0.1:7788` 可在既有 master 上動態加反向轉發；遠端 `curl http://127.0.0.1:17788/api/session` 被 daemon 以 `non-local request` 拒絕（Host 檢查，預期），但 `/hook/claude` 回 `401 unknown bot`（只驗 token，未被 Host 擋下）。
- 遠端 claude 以 `--settings` 注入指向 `hook.sh` 的 hooks，SessionStart 確實觸發腳本並經反向通道打到 daemon；失敗時 spool 寫入 1 行。Stop hook 因遠端帳號當時撞到 session 上限未驗到。
- 實測用的 `hook.sh`（POSIX sh）：

```sh
#!/bin/sh
PROVIDER="$1"; BOT="$2"; TOKEN="$3"; PORT="$4"; shift 4
if [ "$PROVIDER" = "codex" ]; then PAYLOAD="$1"; else PAYLOAD=$(head -c 1048576); fi
NOW=$(date -u +%Y-%m-%dT%H:%M:%SZ)
BODY=$(printf '{"bot_id":"%s","provider":"%s","payload":%s,"received_at":"%s","truncated":false}' "$BOT" "$PROVIDER" "$PAYLOAD" "$NOW")
DIR="$HOME/.config/agents-manager/bots/$BOT"
OUT=$(NO_PROXY=127.0.0.1 curl -s -m 2 --connect-timeout 0.3 -o /dev/null -w '%{http_code}' -X POST "http://127.0.0.1:$PORT/hook/$PROVIDER" \
  -H 'Content-Type: application/json' -H "X-AM-Bot-Token: $TOKEN" --data-binary "$BODY" 2>>"$DIR/hook.log") || OUT=fail
case "$OUT" in 2*) ;; *) printf '%s\n' "$BODY" >> "$DIR/hook-spool.jsonl";; esac
exit 0
```
- 使用者權限的測試 sshd：`/usr/sbin/sshd -f <cfg>`，cfg 指定 `Port 2222`、`ListenAddress 127.0.0.1`、自產 HostKey、`AuthorizedKeysFile`、`StrictModes no`、`AllowStreamLocalForwarding yes`、`StreamLocalBindUnlink yes`、`UsePAM no`；可正常登入與轉發，不需 sudo、不改系統設定。


**額度探測相關（2026-09-06 補測）**：遠端 `/usage` 探測開在 daemon 自己的遠端 session（§14.2）。第一次在
遠端家目錄開 claude 會跳工作區信任對話框，游標在 **No, exit**——先送 Enter 會把 claude 關掉，之後的鍵
全部打進 shell；正確順序是 **Down 再 Enter**，答過一次就不再問。同機若已有另一個 daemon 在管同一台遠端，
兩者會搶同一條 ssh master 與 `/tmp/agents-manager-<uid>/<host>.sock`，症狀是 `ssh master exited` 與
`herdr closed connection without a response (agent.wait)`。

**herdr `pane report-agent` 實測（2026-09-07，本機 herdr 0.8.2，用完即丟的 `am-hookspec` session）**——v4.3 遠端 hook 改走 herdr 事件的依據：

| 觀察 | 結果 |
|---|---|
| `pane report-agent <pane> --source am:test --agent claude --state working/idle --seq N` | 訂閱端收到 `pane.agent_status_changed`，data 只有 `{pane_id, workspace_id, agent, agent_status}` |
| `--message` | **不出現在事件裡**，`api snapshot` 也沒有；不能拿來送內容 |
| `--seq` | 每個 `(pane, source)` 各自單調：`seq=11` 之後送 `seq=5` 不生效；換 `--source` 後從 1 開始就生效 |
| `--agent-session-id` / `pane report-agent-session` | 0.8.2 **不發事件**，`api snapshot` / `agent list` 也看不到（只有 `state_change_seq` 會動）；native session id 仍只能從 payload 取 |
| 狀態同時來自終端偵測 | 兩邊並存；相同狀態不會重發事件，所以「偵測先報 idle」會讓 hook 的上報變成 no-op（§11.4.4 用定時掃描補） |
| pane env | herdr 注入 `HERDR_ENV=1`、`HERDR_PANE_ID`、`HERDR_SESSION`、`HERDR_SOCKET_PATH`、`HERDR_TAB_ID`、`HERDR_WORKSPACE_ID`；hook 用 `HERDR_PANE_ID` 就能自報 |
| herdr 官方 integration（`herdr integration install claude|grok`） | 只在 SessionStart 呼叫 `pane.report_agent_session`，狀態完全交給終端偵測；`seq` 用 `time.time_ns()` |

## 附錄 F：grok CLI 實測（2026-09-06，grok 1.0.13 `5e9a58528b76`，macOS，herdr 0.8.2）

### F.1 CLI 旗標（`grok --help` 節錄）
```
Usage: grok [OPTIONS] [PROMPT] [COMMAND]
      --always-approve            Auto-approve all tool executions
      --permission-mode <MODE>    default | acceptEdits | auto | dontAsk | bypassPermissions | plan
  -m, --model <MODEL>             Model ID to use
      --reasoning-effort <EFFORT> (alias --effort)
  -p, --single <PROMPT>           Single-turn prompt. Prints the response to stdout and exits
      --output-format <FMT>       plain | json | streaming-json | streaming-messages-json（headless）
  -c, --continue / -r, --resume [<ID>] / -s, --session-id <UUID> / --fork-session
      --cwd <CWD> / -w, --worktree [<NAME>] / --worktree-ref <REF>
      --allow <RULE> / --deny <RULE> / --tools <T> / --disallowed-tools <T> / --sandbox <PROFILE>
      --agent <NAME> / --agents <JSON> / --no-subagents / --no-plan / --rules <RULES>
      --system-prompt-override <PROMPT> / --json-schema <SCHEMA> / --max-turns <N> / --verbatim
      --fullscreen / --minimal / --no-alt-screen / --oauth / --debug / --debug-file <FILE>
      --leader-socket <PATH>
Commands: agent, clone, completions, dashboard, doctor, du, export, inspect, leader, login, logout,
          mcp, memory, models, plugin, sessions, setup, trace, update, version, worktree, wrap
```
- `--always-approve` 為 `--permission-mode bypassPermissions` 的別名（文件 14-headless-mode.md：「`--always-approve` (alias `--yolo`, same as `--permission-mode bypassPermissions`)」；但 `--yolo` 不在 1.0.13 的 `--help` 中，保守用 `--always-approve`）。
- **不存在**：`--settings`、`--hooks`、`--plugin-dir`（TUI）、`--trust`（文件提到但 `--help` 無）。`--plugin-dir` 只在 `grok agent [stdio|serve|…]` 子命令。
- `grok models`：`grok-4.6`（預設）、`grok-4.5`。使用者 `~/.grok/config.toml`：`[models] default = "grok-4.6"`、`[ui] permission_mode = "always-approve"`。

### F.2 hook 機制（`~/.grok/docs/user-guide/10-hooks.md` + 實測）
- 來源（全部合併）：`~/.grok/hooks/*.json`（全域、永遠信任）、`~/.claude/settings.json` 相容掃描、`<project>/.grok/hooks/*.json`（需 `/hooks-trust`）、`~/.grok/config.toml` `[[hooks.<Event>]]`、`managed_config.toml`、`requirements.toml`、plugin `hooks/hooks.json`。herdr 的 `herdr integration install grok` 也是寫 `~/.grok/hooks/herdr-agent-state.sh`（本機未安裝）；`~/.grok/hooks/` 現有 `cmux-session.json`（cmux 寫的，格式同 Claude `hooks` 物件）。
- 事件：`SessionStart`、`SessionEnd`、`UserPromptSubmit`、`Stop`（可 block）、`StopFailure`、`StopCancelled`、`PreToolUse`、`PostToolUse`、`Notification`（`idle_prompt` / `permission_prompt`）、`SubagentStart/Stop`、`PreCompact/PostCompact`。
- 執行：command 經 shell 執行，事件 JSON 由 **stdin** 給；runner 注入 env `GROK_HOOK_EVENT`、`GROK_HOOK_NAME`、`GROK_SESSION_ID`、`GROK_WORKSPACE_ROOT`、`CLAUDE_PROJECT_DIR`；**父程序 env 會繼承**（實測 `AM_BOT_ID` / `AM_PORT` 從 pane env 一路傳到 hook）。Stop 預設 timeout 600 秒、其餘 5 秒；失敗 fail-open；Stop 的 stdout JSON 會被當 decision，exit 2 會 block。
- 實測 payload（`-p` 模式與 TUI 相同；鍵為 camelCase，1.0.13 另附 `hook_event_name` / `session_id` / `transcript_path` / `permission_mode` 的 snake_case 副本）：

```json
// session_start（TUI 下延遲到第一次 prompt 才觸發）
{"hookEventName":"session_start","sessionId":"01a072c2-…","cwd":"…","workspaceRoot":"…",
 "timestamp":"2026-09-05T18:09:10.232596+00:00","permissionMode":"bypassPermissions","source":"new"}
// stop（回合結束）
{"hookEventName":"stop","sessionId":"01a072c2-…","cwd":"…","workspaceRoot":"…","timestamp":"…",
 "transcriptPath":"/Users/m1pro/.grok/sessions/%2Fpath%2Fescaped/01a072c2-…/updates.jsonl",
 "promptId":"089f03f9-…","permissionMode":"bypassPermissions","reason":"end_turn",
 "stopHookActive":false,"lastAssistantMessage":"GROK-OK","backgroundTasks":[],"sessionCrons":[]}
// stop（session 結束時再來一次，觀察用）
{"hookEventName":"stop", …, "reason":"shutdown","stopHookActive":false}   // 無 promptId / lastAssistantMessage
// session_end
{"hookEventName":"session_end", …, "reason":"shutdown"}
```
- argv：空（`argv=[]`）；只用 stdin。

### F.3 herdr 測試 session（`herdr --session am-grok server`，用完 `server stop` + `session delete`）
- `agent start am-grok-probe --kind grok --pane w1:p1 -- --always-approve` → **3 秒** `idle`，`argv:["grok","--always-approve"]`，`terminal_title: "grok"`；無 trust 提示（scratch 目錄）。畫面：Grok Build 1.0.13 歡迎框 + 遙測 opt-in banner + `╭ │ ❯ │ ╰ … Grok 4.6 (low) · always-approve ─╯` 輸入框 + `[stable]`。
- `agent prompt … "Reply with exactly GROK-OK" --wait --until idle` → **6 秒**；hook log 依序 `session_start`（prompt 當下）、`stop`（`end_turn`，`lastAssistantMessage: "GROK-OK"`）。
- 回覆在終端的樣子（`visible`，無標記）：
```
     ❯ Reply with exactly GROK-OK                                    2:09 AM
     ◆ user_prompt_submit  [hooks: 1]                                        █
     ◆ Thought for 0.1s                                                      █
     GROK-OK                                                         2:09 AM   █
     Worked for 3.6s                                        stop  [hooks: 2]   █
  Help improve Grok                                       [Opt out] [Opt in]
  Off by default. Opt-in to allow SpaceXAI to retain coding data, e.g., prompts, …
  Read Terms and Privacy Policy.
  ╭──…──╮ / │ ❯ … │ / ╰── Grok 4.6 (low) · always-approve ─╯
  Shift+Tab:mode  │  Ctrl+.:shortcuts
```
- `-p` 模式：`AM_BOT_ID=probe grok -p "Reply with exactly GROK-OK"` 9 秒印出 `GROK-OK`，同樣觸發 session_start / stop(end_turn) / session_end / stop(shutdown)。
- herdr 的 grok manifest（`~/.local/state/herdr/agent-detection/remote/grok.toml`，bundled 2026.07.16.2 更新）：blocked 靠 OSC title 含 `Action Required`、`┃  2 (○) Yes, proceed` 選單、頁尾 `:select │ ctrl+o:yolo │ ctrl+c:cancel`；working 靠 OSC 9;4 `4;1;-1`、braille spinner 行尾 `[stop]`、頁尾 `esc:cancel`；idle 靠 OSC title `grok` / `<session> - grok`、頁尾 `ctrl+.:shortcuts`。

### F.4 身份隔離
- `GROK_HOME`：覆寫設定目錄（預設 `~/.grok`），含 `config.toml`、`auth.json`、`sessions/`、`hooks/`、`plugins/`、`memory/`（05-configuration.md、17-sessions.md、26-config-reference.md）。無 `GROK_CONFIG_DIR`。
- 其他相關 env：`XAI_API_KEY`（API key 登入）、`GROK_SANDBOX`（= `--sandbox`）、`GROK_FOLDER_TRUST=0`（關閉 folder trust）、`GROK_CLAUDE_HOOKS_ENABLED` / `GROK_CURSOR_HOOKS_ENABLED`（相容掃描開關）。
