# Agents Manager 規格書（v3）

> 修訂紀錄
> - v0（2026-09-05）：初稿。
> - v1（2026-09-05）：依 Codex（gpt-6-astra）第一輪審視與本機實測修訂：回覆來源改為 hooks / notify 為主、終端輸出為備援；資料模型拆出 Bot / Run / Conversation / Turn；socket 契約回填正文；補對帳、ownership、送訊息交易語義、本機存取驗證。
> - v2（2026-09-05）：Codex 第二輪（因用量上限中斷，僅取得初步結論）：hook 身分改 per-bot、hook 早於 RPC 回應、備援與晚到 hook 配對、SQLite schema 與里程碑草案。
> - v3（2026-09-05）：依 Grok（grok CLI 1.0.13）第二輪完整審視修訂 15 條：Turn 狀態收斂為 `in_flight`（每 Run 至多一筆，hook 只配那一筆）；per-bot 鎖；active Run 部分唯一索引；先寫 Run 再建 pane；blocked 不觸發備援；hook 子命令最低契約；事件訂閱拓撲實測定案；TOML / SQLite 權威劃分；多項 demo 範圍縮減標為第二階段。

## 1. 目標

一個本機執行的「多 agent 管理器」。使用者透過 Web UI 以聊天方式管理多個正在終端中執行的 coding agent CLI，第一階段支援 **Claude Code** 與 **Codex**。

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
| **Bot** | 使用者定義的 agent 設定：名稱、kind、啟動參數。屬於一個 Project | `bot_id`（ULID，永久） | 無直接對應 |
| **Run** | Bot 的一次執行實例。**每個 Bot 同時最多一個 active Run（DB 部分唯一索引保證）**。欄位含 `native_session_id`、`transcript_path`（由 hook 回填，可為 NULL） | `run_id`（ULID） | `pane_id` + herdr agent `name`（= `bot.name`） |
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
- **hook receiver**：`POST /hook/claude|codex`，驗 per-bot token → **入佇列後立即回 200** → 背景配對（§6.7）。

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

hook 身分為 **per-bot**（`bot_id` + `bots.hook_token`），daemon 解析該 Bot 目前的 active Run。原因：對帳收養會產生新 `run_id`，但存活的 agent 程序仍持有啟動時的參數。pane env 中的 `AM_RUN_ID` **僅供診斷，不作身分**。

### 4.2 回補來源：transcript（第二階段，先留介面）
- `messages.source` 保留 `transcript` 值；`runs.transcript_path` 由 SessionStart 回填。UI「重新載入完整回覆」按鈕 disabled。

### 4.3 備援來源：終端快照
- 觸發：Turn `in_flight` 且 `delivery = ok`，agent 狀態由 `working` 轉為 **`idle`**（`blocked` 不觸發），5 秒內未收到 hook。
- 執行：CAS `UPDATE turns SET status='completed_fallback' WHERE id=? AND status='in_flight'`；成功才 `agent.read {source: recent_unwrapped, lines: 200}`，取上次游標（`last_read_revision` + 已見文字尾端 hash）之後的內容，依 provider 規則抽回覆（Claude：`⏺ ` 開頭；Codex：`• ` 開頭）。
- 存為 assistant Message `source = terminal_fallback`、`incomplete = 1`。**之後晚到的 hook 不覆蓋**（去重後丟棄並 log），避免跨回合錯配。

### 4.4 hook 子命令（`agents-managerd hook claude|codex`）最低契約
1. wall-clock ≤ 3 秒；**永遠 exit 0；永遠空 stdout**（即使錯誤也不印 JSON）。
2. 讀 stdin（Claude）上限 1 MiB，超限截斷並標 `truncated`；Codex 取 argv 最後一個參數。
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
  kind = "claude"            # claude | codex
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
   - `env`：`AM_BOT_ID`、`AM_RUN_ID`（診斷用）、`AM_PORT`、`CLAUDE_CODE_CHILD_SESSION=""`、`CLAUDECODE=""`。
   - 失敗 → Run `exited`（`ended_at` 填入），回 502。
4. 更新 Run 的 `workspace_id` / `pane_id`。產生 hook 注入檔（Claude）或參數（Codex）。
5. `agent.start {name: bot.name, kind, pane_id, args: injected ++ bot.args, timeout_ms: 60000}`（立即回傳 `launch_pending`）。失敗 → Run `exited` + 盡力 `pane.close`。
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

### 6.4 停止 Bot（在 per-bot 鎖內）
- `interrupt`：`agent.send_keys [esc]`，Run 狀態不變。
- `stop`：Run `stopping` → in-flight Turn 標 `failed` → `ctrl+c` ×2（間隔 500 ms）→ 等 `pane.exited` 或 agent 消失最多 10 秒 → 否則 `pane.close` → Run `stopped`（`ended_at`）→ 關閉狀態訂閱。
- DELETE Bot：先 stop → TOML 移除 → DB `deleted_at`，保留 Conversation。
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
   - Claude `SessionStart` / Codex 首次任何事件 → 回填 `runs.native_session_id`、`transcript_path`，**不建 Turn**。
   - Claude `Stop`（`stop_hook_active=false`）/ Codex `agent-turn-complete` → 進入配對。
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
| PATCH | `/bots/:id` | `{args?, autostart?, name?}`；name 變更需無 active Run |
| DELETE | `/bots/:id` | §6.4 |
| POST | `/bots/:id/start` | 200 `{run_id}`；已有 active Run → 409 `{run_id}` |
| POST | `/bots/:id/stop` | 200；無 Run → 204 |
| POST | `/bots/:id/interrupt` | 送 esc |
| POST | `/bots/:id/prompt` | `{text, client_request_id}` → 200 `{turn_id, message_id, delivery}`；409 見 §6.3 |
| POST | `/bots/:id/keys` | `{keys:[...], expect_run_id}`；run 不符 409 |
| POST | `/turns/:id/abandon` | in-flight / delivery=unknown → `failed` |
| GET | `/bots/:id/messages?before=&limit=` | 倒序分頁 |
| GET | `/bots/:id/terminal?source=visible|recent_unwrapped&lines=` | 快照（含 revision / truncated） |

### 7.3 WebSocket `/ws`
- 事件帶遞增 `seq`（記憶體，daemon 重啟從 0）。客戶端帶 `?since=`；daemon 保留最近 200 則，無法補齊或 seq 倒退 → 送 `{"type":"resync"}`，客戶端重新 `GET /state` 與各 bot 訊息。
- 事件：`bot_status`、`message_added`、`turn_updated`、`project_changed`、`bot_changed`、`daemon_status`。`terminal_snapshot` 推送為第二階段（第一階段前端輪詢 `GET terminal`）。

## 8. 技術選型
- Rust：axum 0.8、tokio、serde/serde_json、sqlx 0.8（sqlite）、toml（serde）、ulid、clap、tracing、reqwest（hook 子命令用）、rust-embed（M8）。
- 前端：Vite、React 19、TypeScript、Zustand、自寫 CSS（深淺色）。
- 專案結構：`daemon/`（單一 crate，bin `agents-managerd`，子命令 `serve` / `hook`）、`web/`。

## 9. 非目標（第一階段）
遠端存取、遠端 herdr、xterm.js 串流、bot 互相對話、diff 檢視、共用使用者 default session、transcript 回補、Codex notify chain、`toml_edit` 保註解、WS terminal 推送、未讀計數、Project 刪除時關 workspace。

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
  name TEXT NOT NULL, kind TEXT NOT NULL CHECK (kind IN ('claude','codex')),
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
  created_at TEXT NOT NULL, updated_at TEXT
);
CREATE INDEX messages_conv_time ON messages(conversation_id, created_at);
CREATE INDEX messages_turn ON messages(turn_id);
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
