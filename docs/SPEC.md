# Agents Manager 規格書（v3.6）

> 修訂紀錄
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
| **Project** | 以目錄為單位的分組。目錄路徑正規化（canonical path）後唯一 | `project_id`（ULID） | 一個 `workspace`（本系統建立並記錄 `workspace_id`；對帳發現不存在則設 NULL 並於下次啟動 Bot 時重建） |
| **Bot** | 使用者定義的 agent 設定：名稱、kind、`model`（v3.3，可為空）、啟動參數。屬於一個 Project | `bot_id`（ULID，永久） | 無直接對應 |
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

### 3.2 React 前端

- Vite + React + TypeScript + Zustand。開發期以 Vite proxy 連 daemon；內嵌（rust-embed）為 M8。
- 版面：左側 sidebar 依 Project 分組列出 Bot（狀態燈）；右側為選定 Bot 的聊天視窗。
- 聊天視窗：氣泡（user / assistant / system），底部輸入框；assistant 氣泡顯示來源標籤（hook / terminal-fallback），fallback 標示「可能不完整」。
- `blocked`：顯示終端 `visible` 快照（每 1 秒輪詢），提供按鍵按鈕（Enter / Esc / y / n / ↑ / ↓ / ctrl+c）。
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
- 存為 assistant Message `source = terminal_fallback`、`incomplete = 1`。**之後晚到的 hook 不覆蓋**（去重後丟棄並 log），避免跨回合錯配。

### 4.4 hook 子命令（`agents-managerd hook claude|codex|grok`）最低契約
1. wall-clock ≤ 3 秒；**永遠 exit 0；永遠空 stdout**（即使錯誤也不印 JSON）。
2. 讀 stdin（Claude、grok）上限 1 MiB，超限截斷並標 `truncated`；Codex 取 argv 最後一個參數。
3. POST `http://127.0.0.1:<port>/hook/<provider>`（寫死 IPv4 loopback；設 `NO_PROXY=127.0.0.1`；連線逾時 300 ms、總逾時 2 秒），header `X-AM-Bot-Token`，body `{bot_id, provider, payload, received_at}`。
4. 失敗 → 以 `O_APPEND` 追加一行 JSON 到 `~/.config/agents-manager/bots/<bot_id>/hook-spool.jsonl`；寫入失敗只記自己的 log 檔（`hook.log`），仍 exit 0。
5. `--port` 來自 command 列（不依賴 env）；env `AM_PORT` 為備援。
6. daemon 端 spool 重放：取得 per-bot 鎖 → rename spool 為 `.replaying` → 逐行依 §6.7 處理 → 刪檔 → 釋放鎖。重放期間該 bot 的 HTTP hook 在鎖外等待（因同一把鎖）。

## 5. 設定檔

路徑：`~/.config/agents-manager/config.toml`

```toml
[server]
listen = "127.0.0.1:7788"
herdr_session = "agents-manager"

[[projects]]
id = "01J..."                 # 缺省時 daemon 首次載入自動補寫
path = "/Users/me/project/foo"
label = "foo"

  [[projects.bots]]
  id = "01J..."
  name = "foo-claude"        # herdr agent name：[a-z][a-z0-9_-]{0,31}，全域唯一
  kind = "claude"            # claude | codex | grok
  args = ["--model", "opus"] # 原生參數，接在 daemon 注入參數之後
  autostart = true
```

- 寫回：第一階段以 serde 全量序列化（註解不保留），先寫暫存檔再原子 rename；daemon 內單一 mutex 序列化；mtime 與載入時不符回 409。`toml_edit` 保留註解為第二階段。
- 改 `name` 時若有 active Run 拒絕（herdr agent name 綁定啟動時的名稱）。
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
3. 取得 pane：
   - 若 workspace 剛由本步驟建立 → 用 `root_pane`。
   - 否則 `pane.split {target_pane_id: <該 workspace 任一 pane>, direction:"right", cwd, focus:false, env}`。
   - `env`：`AM_BOT_ID`、`AM_RUN_ID`（診斷用）、`AM_PORT`、`AM_HOOK_TOKEN`（v3.6；`inject_hooks = false` 時不給，grok 的分派腳本以此判斷是否回報）、`CLAUDE_CODE_CHILD_SESSION=""`、`CLAUDECODE=""`。
   - 失敗 → Run `exited`（`ended_at` 填入），回 502。
4. 更新 Run 的 `workspace_id` / `pane_id`。產生 hook 注入檔（Claude）或參數（Codex）。
5. 先寫 `runs.agent_name = agent_name(project.label, bot.name)`，再 `agent.start {name: <agent_name>, kind, pane_id, args: injected ++ bot.args, timeout_ms: 60000}`（立即回傳 `launch_pending`）。之後所有 herdr 目標（wait / prompt / keys / stop）一律用 `run.agent_name`，缺值時退回 `bot.name`。失敗 → Run `exited` + 盡力 `pane.close`。
6. 開該 pane 的狀態訂閱連線。
7. `agent.wait {until:[idle,done,blocked], timeout_ms: 60000}`：
   - `idle/done` → Run `running`，agent `idle`。
   - `blocked` → Run `running`，agent `blocked`（例如 trust 提示），UI 顯示終端。
   - timeout / error → **不**關 pane；`agent.get` 若有 agent → Run `running` / `unknown`；若無 → Run `exited` + `pane.close`。

### 6.3 送訊息（在 per-bot 鎖內，單一 DB 交易）
1. 檢查：Run `running`；agent 狀態 ≠ `blocked`；該 Run 無 `in_flight` Turn；無 `delivery=unknown` 的 Turn → 否則 409（body 含原因與既有 `turn_id`）。
2. 冪等：`client_request_id` 已存在 → 回同一 `turn_id`（200）。
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
- 重啟 Bot（v3.3）：`POST /bots/:id/restart` = 有 Run 就先 stop 再 start，用來套用改過的 `model` / `args` / `identity` / `env`。
- DELETE Project：需所有 Bot 已停止 → TOML 移除 → 不關 workspace（第二階段）、不刪目錄。

### 6.5 對帳（daemon 啟動、事件連線重連；逐 bot 在鎖內）
1. `session.snapshot` + `agent.list`。
2. DB 中 active Run：
   - agent 清單中有 `name == bot.name` → 維持，更新 `pane_id`（pane move 會改 id）與 `agent_status`。
   - 否則 → Run `exited`。
3. agent 清單中 `name` 匹配某 Bot 但 DB 無 active Run → 建 Run（`running`，`adopted=1`），沿用同一 Conversation。
4. **orphan pane 回收**（第一階段）：DB 中 `exited/stopped` Run 記錄的 `pane_id` 若仍存在於 snapshot 且無 agent → `pane.close`。
5. 重建各 active Run 的狀態訂閱。

### 6.6 事件處理
- `pane.agent_status_changed`：更新 Run `agent_status`；`working→idle` 啟動備援計時（§4.3）；推 WS。
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
   - 有 → 建 assistant Message（`source=hook`），Turn `completed`，寫入 native ids。
   - 無 → 第 5 點。
5. external：建 Turn（`origin=external`, `status=completed`）+ user Message（Codex 可從 `input-messages` 取得；Claude 無則省略）+ assistant Message。
6. 推 WS `message_added` / `turn_updated`。

## 7. API

### 7.1 存取控制
- bind `127.0.0.1`。
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
遠端存取、遠端 herdr、xterm.js 串流、bot 互相對話（使用者對多個 bot 的群組發言見 §13）、diff 檢視、共用使用者 default session、transcript 回補、Codex notify chain、`toml_edit` 保註解、WS terminal 推送、未讀計數（§13 群組視圖的前端記憶體計數除外）、Project 刪除時關 workspace。


## 11. 遠端主機（Remote hosts，v3.1）

### 11.1 目標
Project 可以位於另一台機器：該機器上有自己的 herdr，agent 在那台機器的 pane 內執行，daemon 仍在本機，UI 操作方式完全相同。herdr 本身的 `--remote` 只支援 TUI attach，因此本系統以 **OpenSSH 轉發**達成（附錄 E 已實測）：

```
本機 daemon ──(ssh -M master)──► 遠端 sshd
   │  -L <本機短路徑>.sock : ~/.config/herdr/sessions/<session>/herdr.sock   （herdr RPC / 事件）
   │  -R <hook_port> : 127.0.0.1:<daemon port>                               （遠端 hook 回呼）
   └─ ssh <host> '<sh 指令>'                                                 （放 hook 腳本、settings、讀 spool、列目錄）
```

### 11.2 設定
```toml
[[hosts]]
name = "m4p"                       # 唯一識別，[a-z][a-z0-9_-]{0,31}
ssh = "m4p@100.112.229.82"         # ssh 目標；可含 ssh_config 別名；port 以 ssh_port 指定
ssh_port = 22
herdr_session = "agents-manager"   # 遠端 named session（絕不使用遠端 default session）
remote_path = "/opt/homebrew/bin:$HOME/.local/bin"   # 非互動 ssh shell 缺少的 PATH，前置到 PATH
hook_port = 7788                   # 遠端 127.0.0.1 上反向轉發的埠；與 daemon 埠相同即可，衝突時改

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
2. **master 連線**：`ssh -N -M -S <ctl> -o BatchMode=yes -o ExitOnForwardFailure=yes -o ServerAliveInterval=15 -o ServerAliveCountMax=3 -o StreamLocalBindUnlink=yes -L <local.sock>:<remote herdr.sock> -R <hook_port>:127.0.0.1:<daemon port> <target>`。
   - `<local.sock>` 與 `<ctl>` 必須放在**短路徑**（macOS AF_UNIX 上限 104 bytes）：`/tmp/agents-manager-<uid>/<host>.sock`、`<host>.ctl`。
3. 以 `HerdrClient::new(<local.sock>)` 取得與本機完全相同的 client；`ping` 成功 → `connected`。
4. 健康檢查：每 10 秒 `ping`；失敗或 master 程序退出 → 標 `disconnected`、指數退避（1s→30s）重建 master → 成功後對該 host 執行對帳（§6.5）並重建事件訂閱。
5. daemon 退出時關閉 master（`ssh -O exit`）；遠端 herdr server 與 agent 保持存活。
6. `App` 由單一 `herdr` 改為 `hosts: HashMap<String, HostConn>`，`"local"` 為既有本機 client；所有用到 `app.herdr` 的地方改為 `app.herdr_for(project.host)`。pane watcher、fallback timer 等以 `(host, pane_id)` 為鍵。對帳與全域事件訂閱逐 host 執行。

### 11.4 遠端 hook
遠端沒有 `agents-managerd` 二進位，改用 **POSIX sh + curl** 腳本（附錄 E 已實測可通過反向通道打到 daemon）：
- daemon 在啟動 Run 時透過 ssh 寫入遠端 `~/.config/agents-manager/bots/<bot_id>/hook.sh`（內容固定，見附錄 E）與 `claude-settings.json`；`chmod +x`。
- claude args：`--settings <遠端絕對路徑>`；codex：`-c notify=["<遠端 hook.sh>","codex","<bot_id>","<token>","<hook_port>"]`；grok（v3.6）：不加 args，另寫遠端 `~/.config/agents-manager/grok-hook.sh`（分派到 `bots/$AM_BOT_ID/hook.sh grok …`）與 `<GROK_HOME>/hooks/agents-manager.json`（§12.2）。`hook.sh` 對 provider ≠ codex 一律讀 stdin，grok 不需改。
- 腳本契約與 §4.4 相同：≤3 秒、exit 0、空 stdout、失敗寫遠端 `hook-spool.jsonl`。
- daemon 的 `/hook/*` **不做** Host 檢查（只驗 per-bot token），因為經反向通道的請求 Host 為 `127.0.0.1:<hook_port>`。
- spool 重放：對帳時 `ssh <host> 'f=~/.config/agents-manager/bots/<id>/hook-spool.jsonl; [ -f "$f" ] && mv "$f" "$f.replaying" && cat "$f.replaying" && rm "$f.replaying"'`，逐行依 §6.7 處理。

### 11.5 目錄選擇器
`GET /api/fs/dirs?host=<name>&path=` 對遠端執行一段 sh：`cd <path> && pwd && for d in */ .[!.]*/; do [ -d "$d" ] && printf '%s\t%s\n' "${d%/}" "$([ -d "$d/.git" ] && echo 1 || echo 0)"; done`，daemon 解析後回傳與本機相同的 JSON（`home` 以 `echo $HOME` 取得；`~` 前綴展開）。隱藏目錄仍略過。

### 11.6 API 與 UI
- `GET /api/state` 新增 `hosts: [{name, ssh, herdr_session, connected, error?}]`；`projects[].host`（`"local"` 或 host name）。
- `POST /api/hosts {name, ssh, ssh_port?, herdr_session?, remote_path?, hook_port?}` → 寫 TOML、立即嘗試連線、回 `{name, connected, error?}`；`DELETE /api/hosts/:name`（需無 project 使用）；`POST /api/hosts/:name/reconnect`。
- `POST /api/projects` 新增 `host?`。
- WS `daemon_status` 改為 `{herdr_connected, hosts: {<name>: {connected, error?}}}`；`host_changed {name, connected, error?}`。
- UI：sidebar Project 標題顯示 host 徽章（本機不顯示）；新增 Project 表單多一個「主機」下拉（本機 + 已設定 hosts），選擇器隨主機切換；新增「主機」管理表單（名稱、ssh 目標、port、session、remote_path），列出各 host 連線狀態與重連按鈕；host 斷線時該 host 的 bot 燈號為 `disconnected`（灰）。

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
     遠端版改為 `exec "$HOME/.config/agents-manager/bots/$AM_BOT_ID/hook.sh" grok "$AM_BOT_ID" "$AM_HOOK_TOKEN" "${AM_PORT:-7788}"`。
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
- 遠端 host：`REMOTE_HOOK_SH` 對 provider ≠ codex 讀 stdin，grok payload 以 `{` 開頭 → 原樣塞進 body，不需第三種分支。

### 12.4 身份隔離
`GROK_HOME`（預設 `~/.grok`）等同 Claude 的 `CLAUDE_CONFIG_DIR`：config.toml、`auth.json`、`sessions/`、`hooks/` 全部跟著走。identity 設 `GROK_HOME=$HOME/.grok-work` 即可用另一個帳號；daemon 會把 hooks 檔寫到該 `GROK_HOME/hooks/`。

### 12.5 資料層
- `bots.kind` CHECK 改為 `('claude','codex','grok')`。SQLite 無法修改 CHECK，舊 DB 以 `bots_new` 重建（`PRAGMA foreign_keys=OFF; legacy_alter_table=ON`，與 projects 的重建同法，並處理中途當機殘留的 `bots_new`）。
- `identities[].kind` 亦允許 `grok`。
- API：`POST /projects/:id/bots` / `POST /identities` 的 kind 驗證改為 `claude | codex | grok`，錯誤訊息 `kind must be claude, codex or grok`。

### 12.6 額度：`/usage` 探測

grok CLI 沒有 `usage` 子命令，也沒有可查額度的 RPC；數字只存在 TUI 的 `/usage` 對話框裡。daemon 因此開一個**用完即丟**的 herdr workspace 探測：

1. `workspace.create`（`focus:false`，label `am-quota-grok`，cwd = 家目錄）
2. `agent.start` kind `grok`、無額外 argv，名稱 `amquota<6碼>`（不在 DB 裡，對帳永遠不會把它當成 bot）
3. `agent.wait` 到 `idle|working|blocked`，再等 3 秒讓輸入列畫好
4. `pane.send_text "/usage"` → 0.8 秒 → `pane.send_keys ["Enter"]`
5. 每 0.9 秒 `pane.read visible 120`，最多 25 秒，直到畫面解析得出額度
6. `workspace.close`（放在 `Drop` 裡，錯誤路徑也會關）

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
| R3 | 遠端 hook | 送 prompt → 回覆來源 `hook`；daemon 停機時遠端 spool 增加一行，重啟後補入 |
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
