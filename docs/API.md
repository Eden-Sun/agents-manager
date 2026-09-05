# agents-managerd HTTP / WebSocket API

daemon 預設 `http://127.0.0.1:7788`（`config.toml` 的 `server.listen`）。本文件是 SPEC §7 的具體定案，
前端請以此為準。所有時間欄位皆為 RFC3339 UTC 字串（毫秒精度）。所有 id 為 ULID 字串。

## 0. 認證

1. `GET /api/session` — **不需 token**，但 daemon 會檢查 `Host` 必須是 `127.0.0.1:<port>` /
   `localhost:<port>` / `[::1]:<port>`，且 `Origin`（若有）為本機。
   ```json
   { "token": "6a03b0754e3b9d333aa7d79363cab160", "port": 7788 }
   ```
2. 其餘 `/api/*` 需 header `X-AM-Token: <token>`。缺或錯 → `401 {"error":"missing or bad X-AM-Token"}`。
3. WebSocket：`/ws?token=<token>`（可加 `&since=<seq>`）。
4. `/hook/*` 用 per-bot 的 `X-AM-Bot-Token`，前端不會用到。

> 開發期 Vite proxy 需把 `/api`、`/ws`（含 WebSocket upgrade）、`/hook` 轉到 `127.0.0.1:7788`。
> 因為 daemon 會檢查 `Host`，proxy 請開 `changeOrigin: true`（Vite 預設會改寫 Host 為 target）。

## 1. 錯誤格式

| 狀態碼 | body | 意義 |
|---|---|---|
| 400 | `{"error":"bad_request","message":"..."}` | 參數錯誤 |
| 401 | `{"error":"..."}` | token 錯 |
| 403 | `{"error":"..."}` | Host / Origin 非本機 |
| 404 | `{"error":"not_found","what":"bot"\|"project"\|"run"\|"pane"\|"turn"}` | 找不到 |
| 409 | `{"error":"conflict","reason":"<人類可讀>", ...extra}` | 狀態機衝突，extra 視情況含 `run_id` / `turn_id` / `bot_id` / `name` / `path` / `state` |
| 502 | `{"error":"upstream","message":"..."}` | herdr / DB 出錯 |

## 2. `GET /api/state`

一次取回整棵樹。前端啟動、收到 `resync` 或 `project_changed` / `bot_changed` 時重新拉。

```json
{
  "daemon_seq": 6,
  "connected": true,
  "herdr_session": "agents-manager",
  "projects": [
    {
      "id": "01M1S2SQPS9TA1DYRKNYCF2SJK",
      "path": "/Users/m1pro/project/agents-manager",
      "label": "agents-manager",
      "workspace_id": null,
      "bots": [
        {
          "id": "01M1S2SQPSYMQ8B1VQ50R963B9",
          "project_id": "01M1S2SQPS9TA1DYRKNYCF2SJK",
          "name": "am-claude",
          "kind": "claude",
          "args": [],
          "autostart": false,
          "inject_hooks": true,
    "auto_approve": true,
          "run": null,
          "lamp": "offline",
          "unread": 0
        }
      ]
    }
  ]
}
```

### `run` 物件（`null` = 目前沒有 active Run）

```json
{
  "id": "01M1...", "bot_id": "01M1...",
  "state": "starting" | "running" | "stopping" | "stopped" | "exited",
  "agent_status": "idle" | "working" | "blocked" | "unknown",
  "workspace_id": "w1", "pane_id": "w1:p2", "adopted": 0,
  "native_session_id": null, "transcript_path": null,
  "last_read_revision": null, "last_read_tail_hash": null,
  "started_at": "2026-09-05T15:30:00.000Z", "ended_at": null
}
```

### `lamp`（合成燈號，SPEC §2.2；前端直接用即可）

| 值 | 建議顏色 | 條件 |
|---|---|---|
| `disconnected` | 灰 | daemon 與 herdr socket 斷線 |
| `offline` | 離線灰 | 無 active Run |
| `starting` | 黃閃 | run.state = starting |
| `stopping` | 黃 | run.state = stopping |
| `idle` | 綠 | running + idle |
| `working` | 藍動畫 | running + working |
| `blocked` | 紅 | running + blocked |
| `unknown` | 灰黃 | running + unknown |

`unread` 第一階段固定為 `0`。

## 3. 設定變更（寫回 config.toml）

| 方法 | 路徑 | body | 回應 |
|---|---|---|---|
| POST | `/api/projects` | `{"path":"/abs/or/~/path","label":"foo"}`（`label` 可省，預設取目錄名） | `200 {"project_id":"..."}`；路徑不存在 400；重複 409 |
| DELETE | `/api/projects/{id}` | — | `200 {}`；仍有 bot 有 active Run → 409 |
| POST | `/api/projects/{id}/bots` | `{"name":"foo-claude","kind":"claude"\|"codex","args":[],"autostart":false,"inject_hooks":true}` | `200 {"bot_id":"..."}`；名稱不合 `[a-z][a-z0-9_-]{0,31}` → 400；名稱重複 409 |
| PATCH | `/api/bots/{id}` | `{"args"?:[],"autostart"?:bool,"name"?:"...","inject_hooks"?:bool}` | `200 {}`；改名時有 active Run → 409 |
| DELETE | `/api/bots/{id}` | — | `200 {}`（會先 stop；conversation 保留） |

這些操作成功後 daemon 會推 `project_changed` / `bot_changed`，前端收到後重新 `GET /api/state`。

## 4. Run 控制

| 方法 | 路徑 | body | 回應 |
|---|---|---|---|
| POST | `/api/bots/{id}/start` | — | `200 {"run_id":"..."}`；已有 active Run → `409 {"error":"conflict","reason":"active run already exists","run_id":"..."}`；herdr 失敗 502 |
| POST | `/api/bots/{id}/stop` | — | `200 {}`；本來就沒有 Run → `204`（無 body） |
| POST | `/api/bots/{id}/interrupt` | — | `200 {}`（送 `esc`，並把 in-flight Turn 標 failed） |
| POST | `/api/bots/{id}/keys` | `{"keys":["y"],"expect_run_id"?:"..."}` | `200 {}`；`expect_run_id` 與現行 Run 不符 → 409 |
| POST | `/api/turns/{id}/abandon` | — | `200 {}`；該 Turn 既非 in-flight 也非 delivery=unknown → 409 |

`keys` 可用的鍵名由 herdr 驗證，常用：`enter`、`esc`、`y`、`n`、`up`、`down`、`ctrl+c`。

## 5. 送訊息

`POST /api/bots/{id}/prompt`

```json
{ "text": "Reply with exactly PONG", "client_request_id": "<前端產生的唯一字串>" }
```

`client_request_id` 可省（daemon 會補），但**建議前端自己帶**以取得冪等：同一個 id 重送會回同一個
`turn_id`，不會重複送給 agent。

成功 `200`：
```json
{ "turn_id": "01M1...", "message_id": "01M1...", "delivery": "ok" | "unknown" | "failed" }
```

- `delivery = "ok"`：已送達 agent，等 hook 回覆（或 5 秒後的終端備援）。
- `delivery = "unknown"`：RPC 逾時，**該 bot 在 abandon / interrupt / stop 之前不能再送 prompt**
  （再送會 409），UI 應顯示「送出狀態不明」並提供「放棄這回合」按鈕（呼叫 `/api/turns/{id}/abandon`）。
- `delivery = "failed"`：agent 當下處於 blocked，Turn 直接標 failed。

409 的 `reason` 可能是：`bot has no active run`、`run is not running`、
`agent is blocked; answer the prompt first`、`a turn is already in flight`、
`a previous turn has unknown delivery; abandon it first`。

## 6. 讀訊息

`GET /api/bots/{id}/messages?before=<message_id>&limit=100`

倒序分頁（`before` 傳目前最舊一則的 `id`），但回傳的 `messages` 已**依時間正序**排好，可直接 append/prepend。

```json
{
  "bot_id": "01M1...",
  "conversation_id": "01M1...",
  "messages": [
    {
      "id": "01M1...", "conversation_id": "01M1...", "turn_id": "01M1...",
      "role": "user" | "assistant" | "system",
      "content": "…",
      "source": "web" | "hook" | "transcript" | "terminal_fallback" | "system",
      "incomplete": 0,
      "terminal_snapshot": null,
      "created_at": "2026-09-05T15:31:00.000Z",
      "updated_at": null
    }
  ],
  "turns": [
    {
      "id": "01M1...", "conversation_id": "01M1...", "run_id": "01M1...",
      "origin": "web" | "external",
      "status": "in_flight" | "completed" | "completed_fallback" | "failed",
      "delivery": "pending" | "ok" | "unknown" | "failed",
      "client_request_id": "...", "native_session_id": null, "native_turn_id": null,
      "created_at": "...", "completed_at": null
    }
  ],
  "has_more": false
}
```

`turns` 為最近 `limit+1` 筆（時間倒序），用來判斷「這回合還在跑」與顯示 delivery 警示。

UI 標籤建議：
- `source = "hook"` → 不加標籤（正常回覆）
- `source = "terminal_fallback"` 或 `incomplete = 1` → 標「終端備援 · 可能不完整」
- `source = "system"` → 系統列（灰字）

## 7. 終端快照

`GET /api/bots/{id}/terminal?source=visible&lines=200`

`source ∈ visible | recent | recent_unwrapped | detection`（預設 `visible`），`lines` 1–2000（預設 200）。
無 active Run → 404。

```json
{
  "bot_id": "01M1...", "run_id": "01M1...", "pane_id": "w1:p2",
  "source": "visible", "text": "……純文字，已去 ANSI……",
  "revision": 42, "truncated": false,
  "agent_status": "blocked"
}
```

第一階段前端在 `lamp = "blocked"` 時每 1 秒輪詢此端點，並提供按鍵按鈕打 `/api/bots/{id}/keys`。

## 8. WebSocket `/ws`

連線：`ws://127.0.0.1:7788/ws?token=<token>`，重連時帶 `&since=<最後收到的 seq>`。

每則訊息是一行 JSON：

```json
{ "seq": 12, "type": "bot_status", "data": { ... } }
```

`seq` 從 1 遞增（daemon 重啟歸零）。daemon 保留最近 200 則；若 `since` 無法補齊或 seq 倒退，
會先送 **不帶 seq 的**：

```json
{ "type": "resync", "seq": 12 }
```

收到 `resync` 就重新 `GET /api/state` 與各 bot 的 messages。

### 事件型別

| type | data |
|---|---|
| `bot_status` | `{"bot_id":"...", "host":"local"\|"<name>", "run": <run 物件或 null>, "connected": true}`（`connected` 是**該 bot 所屬 host** 的連線狀態） |
| `message_added` | `{"bot_id":"...", "message": <message 物件>}` |
| `turn_updated` | `{"bot_id":"...", "turn": <turn 物件>}` |
| `project_changed` | `{"project_id":"..."}`（或 `{}`） |
| `bot_changed` | `{"bot_id":"..."}` |
| `daemon_status` | `{"connected": true\|false}` — daemon 與 herdr 的連線狀態 |

範例：

```json
{"seq":31,"type":"bot_status","data":{"bot_id":"01M1...","connected":true,"run":{"id":"01M1...","bot_id":"01M1...","state":"running","agent_status":"working","workspace_id":"w1","pane_id":"w1:p2","adopted":0,"native_session_id":"3f2c…","transcript_path":"/Users/…/.claude/projects/…/3f2c….jsonl","last_read_revision":null,"last_read_tail_hash":null,"started_at":"2026-09-05T15:40:02.113Z","ended_at":null}}}
{"seq":32,"type":"message_added","data":{"bot_id":"01M1...","message":{"id":"01M1...","conversation_id":"01M1...","turn_id":"01M1...","role":"assistant","content":"PONG","source":"hook","incomplete":0,"terminal_snapshot":null,"created_at":"2026-09-05T15:40:09.882Z","updated_at":null}}}
{"seq":33,"type":"turn_updated","data":{"bot_id":"01M1...","turn":{"id":"01M1...","conversation_id":"01M1...","run_id":"01M1...","origin":"web","status":"completed","delivery":"ok","client_request_id":"c-1","native_session_id":"3f2c…","native_turn_id":"prompt_01…","created_at":"2026-09-05T15:40:05.001Z","completed_at":"2026-09-05T15:40:09.880Z"}}}
{"seq":34,"type":"daemon_status","data":{"connected":false}}
```

`terminal_snapshot` 推送為第二階段；第一階段請輪詢 §7。

## 9. 前端流程建議

1. `GET /api/session` 取 token（存記憶體即可）。
2. `GET /api/state` 畫 sidebar（依 project 分組）。
3. 開 `/ws?token=…`，收到 `bot_status` 更新燈號、`message_added` / `turn_updated` 更新聊天視窗；
   `project_changed` / `bot_changed` / `resync` → 重新 `GET /api/state`。
4. 選定 bot 時 `GET /api/bots/{id}/messages?limit=100`。
5. 送訊息用 `POST /api/bots/{id}/prompt`（自帶 `client_request_id`），user 氣泡會同時經
   `message_added` 推回來——請用 `message.id` 去重，不要用本地暫存氣泡重複顯示。


## GET /api/fs/dirs?path=

目錄瀏覽（新增 Project 的目錄選擇器用）。`path` 省略或空白時為家目錄；支援 `~` 前綴。只列子目錄，略過 `.` 開頭項目；symlink 指向目錄者也列出。

```json
{"path":"/Users/me/project","parent":"/Users/me","home":"/Users/me",
 "entries":[{"name":"foo","path":"/Users/me/project/foo","git":true}]}
```

錯誤：路徑不存在或不是目錄 → 400 `{"error":"bad_request","message":"..."}`。


## bot.auto_approve（2026-09-06 新增）

每個 bot 的布林欄位，預設 `true`。啟動時 daemon 依 kind 注入略過權限確認的旗標：claude `--dangerously-skip-permissions`、codex `--yolo`（等同 `--dangerously-bypass-approvals-and-sandbox`）。`POST /projects/:id/bots` 與 `PATCH /bots/:id` 皆接受 `auto_approve`。舊資料庫啟動時自動 `ALTER TABLE` 補欄位。


## 遠端主機 hosts（SPEC §11.6，2026-09-06 新增）

Project 可位於另一台機器。daemon 仍在本機，透過 SSH 轉發連到遠端 herdr。UI 操作方式完全相同，
差別只在 Project 多了一個 `host` 欄位，以及多了一組 `/api/hosts` 端點。

`host` 是 host 名稱字串，`"local"` 為保留字，代表本機（不可被使用者建立/刪除）。

### `GET /api/state` 新增欄位

```json
{
  "daemon_seq": 6,
  "connected": true,
  "herdr_session": "agents-manager",
  "hosts": [
    {"name":"local","ssh":null,"ssh_port":null,"ssh_opts":[],"herdr_session":"agents-manager",
     "remote_path":null,"hook_port":null,"connected":true,"error":null},
    {"name":"m4p","ssh":"m4p@100.112.229.82","ssh_port":22,"ssh_opts":[],
     "herdr_session":"agents-manager","remote_path":"/opt/homebrew/bin:$HOME/.local/bin",
     "hook_port":7788,"connected":false,"error":"ssh master exited immediately (exit status: 255)"}
  ],
  "projects": [
    {"id":"01M1...","path":"/Users/m4p/work/foo","label":"foo@m4p","host":"m4p",
     "workspace_id":null,"bots":[ ... ]}
  ]
}
```

- `hosts` **必定含 `local`**，且 `local` 永遠排在第一個；`local` 的 `ssh` / `ssh_port` /
  `remote_path` / `hook_port` 為 `null`，`connected` = 本機 herdr 連線狀態（與頂層 `connected` 同值）。
- UI 的「主機」下拉可直接用這個陣列；sidebar 的 host 徽章在 `project.host !== "local"` 時才顯示。
- `error` 為 `null` 或人類可讀的錯誤字串（ssh 認證失敗、herdr 起不來、ping 失敗…）。
- `projects[].host` 永遠存在，本機專案為 `"local"`。

### bot `lamp`

host 斷線時（`hosts[].connected = false`），該 host 底下所有 bot 的 `lamp` 一律為 `"disconnected"`
（灰），不論 run 狀態為何。本機 bot 的行為不變（沿用頂層 `connected`）。

### `POST /api/hosts`

新增或更新一台遠端主機。寫回 `config.toml` 的 `[[hosts]]`，接著**立即**嘗試連線後才回應
（含 ensure remote session + ssh master + ping，最多約 20 秒）。

```json
{
  "name": "m4p",
  "ssh": "m4p@100.112.229.82",
  "ssh_port": 22,
  "herdr_session": "agents-manager",
  "remote_path": "/opt/homebrew/bin:$HOME/.local/bin",
  "hook_port": 7788,
  "ssh_opts": ["-i", "/path/to/key"]
}
```

| 欄位 | 必填 | 預設 |
|---|---|---|
| `name` | ✅ | —，須符合 `[a-z][a-z0-9_-]{0,31}`，`"local"` 保留 |
| `ssh` | ✅ | — ，`user@host` 或 ssh_config 別名 |
| `ssh_port` | | `22` |
| `herdr_session` | | `"agents-manager"` |
| `remote_path` | | `""`（非互動 ssh shell 缺少的 PATH，會前置到遠端 PATH） |
| `hook_port` | | daemon 自己的 port（預設 7788） |
| `ssh_opts` | | `[]`，額外的 ssh 參數，原樣附加到每個 ssh 指令（例：`["-i","~/.ssh/id_x"]`） |

回應 `200`（同步等待第一次連線結果，最長約 35 秒）：

```json
{ "name": "m4p", "connected": true, "error": null }
```

連不上時仍回 `200`（設定已寫入），`connected: false` 且 `error` 有字串；UI 應顯示錯誤並提供重連。
名稱不合法或為 `local` → `400`。已存在同名 host → 視為更新（會先斷開舊連線再以新設定連）。

### `DELETE /api/hosts/{name}`

`200 {}`。仍有 project 使用該 host → `409 {"error":"conflict","reason":"host still used by projects","project_id":"..."}`。
`name = "local"` → `400`。

### `POST /api/hosts/{name}/reconnect`

強制斷開並重建 ssh master 與訂閱，回應同 `POST /api/hosts`：

```json
{ "name": "m4p", "connected": true, "error": null }
```

`local` 亦可呼叫（重新 ping 本機 herdr）。找不到 host → `404`。

### `POST /api/projects` 新增 `host`

```json
{ "path": "/Users/m4p/work/foo", "label": "foo@m4p", "host": "m4p" }
```

`host` 可省（預設 `"local"`）。指定未設定的 host → `404 {"error":"not_found","what":"host"}`。
**遠端 project 的 `path` 不做本機 canonicalize**（本機不存在該路徑）：daemon 會透過 ssh 檢查該目錄
存在並取得遠端 canonical path，失敗 → `400`。

### `GET /api/fs/dirs?host=<name>&path=<path>`

`host` 可省（預設 `local`）。指定 host 時，daemon 透過 ssh 列出**遠端**目錄，回傳格式與本機完全相同：

```json
{"path":"/Users/m4p/work","parent":"/Users/m4p","home":"/Users/m4p",
 "entries":[{"name":"foo","path":"/Users/m4p/work/foo","git":true}]}
```

host 不存在 → `404`；ssh 失敗（含認證失敗、目錄不存在）→ `502 {"error":"upstream","message":"..."}`。

> 目錄瀏覽只用 ssh，不經過 herdr，所以 **host 斷線（`connected:false`）時仍可能成功回 200**；
> 只有 ssh 本身失敗才回 502。UI 可以在 host 斷線時照樣讓使用者瀏覽目錄。

### WebSocket 事件變更

| type | data |
|---|---|
| `daemon_status` | `{"herdr_connected":true,"connected":true,"hosts":{"local":{"connected":true,"error":null},"m4p":{"connected":false,"error":"..."}}}` |
| `host_changed` | `{"name":"m4p","connected":true,"error":null}` |

- `daemon_status` 的 `connected` 保留為 `herdr_connected` 的同義欄位（相容舊前端），新程式請用
  `herdr_connected`；`hosts` 是 map，key 為 host name（含 `local`）。
- `host_changed` 在 host 連上 / 斷線 / 新增 / 刪除 / 設定變更時推送。刪除時送
  `{"name":"m4p","connected":false,"error":"removed"}`，並同時推 `project_changed`。
- host 連線狀態改變時，該 host 底下每個 bot 也會收到 `bot_status`（`lamp` 已反映 disconnected）。
