# agents-managerd HTTP / WebSocket API

daemon 預設 `http://127.0.0.1:7788`（`config.toml` 的 `server.listen`）。本文件是 SPEC §7 的具體定案，
前端請以此為準。所有時間欄位皆為 RFC3339 UTC 字串（毫秒精度）。所有 id 為 ULID 字串。

## 0. 認證

1. `GET /api/session` — **不需 token**，但 daemon 會檢查 `Host` 必須是 `127.0.0.1:<port>` /
   `localhost:<port>` / `[::1]:<port>`，且 `Origin`（若有）為本機。
   ```json
   { "token": "<32 hex chars>", "port": 7788 }
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
  "default_connected": true,
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
          "model": null,
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
| PATCH | `/api/projects/{id}` | `{"label":"新名字"}` | `200 {"project_id":"...","needs_restart":false}`；label trim 後為空 → 400。不擋 active run：herdr 的 agent 身分取自 bot id，label 只影響 legacy 名稱與**下次啟動**的 `agent_name` slug |
| POST | `/api/projects/{id}/bots` | `{"name":"foo-claude","kind":"claude"\|"codex"\|"grok","args":[],"autostart":false,"inject_hooks":true}` | `200 {"bot_id":"..."}`；名稱不合 `[a-z][a-z0-9_-]{0,31}` → 400；名稱重複 409 |
| PATCH | `/api/bots/{id}` | `{"name"?,"model"?,"args"?,"autostart"?,"auto_approve"?,"inject_hooks"?,"identity"?,"env"?}` | `200 {"needs_restart":bool}`；改名時有 active Run → 409（詳見 §10） |
| DELETE | `/api/bots/{id}` | — | `200 {}`（會先 stop；conversation 與訊息保留，詳見 §10） |

這些操作成功後 daemon 會推 `project_changed` / `bot_changed`，前端收到後重新 `GET /api/state`。

## 4. Run 控制

| 方法 | 路徑 | body | 回應 |
|---|---|---|---|
| POST | `/api/bots/{id}/start` | — | `200 {"run_id":"..."}`；已有 active Run → `409 {"error":"conflict","reason":"active run already exists","run_id":"..."}`；herdr 失敗 502 |
| POST | `/api/bots/{id}/stop` | — | `200 {}`；本來就沒有 Run → `204`（無 body） |
| POST | `/api/bots/{id}/interrupt` | — | `200 {}`（送 `esc`，並把 in-flight Turn 標 failed）；送不出 `esc` → 502，Turn **維持** in-flight |
| POST | `/api/bots/{id}/abort` | — | `200 {"aborted":["<turn_id>",…],"keys_sent":true,"key_error":null}` — **強制**結束目前回合，見 §4.2 |
| POST | `/api/bots/{id}/keys` | `{"keys":["y"],"expect_run_id"?:"..."}` | `200 {}`；`expect_run_id` 與現行 Run 不符 → 409 |
| POST | `/api/bots/{id}/text` | `{"text":"多行\n也可以","enter"?:true,"expect_run_id"?:"..."}` | `200 {}`；Run 沒有 pane → 404；`expect_run_id` 不符 → 409 |
| POST | `/api/turns/{id}/abandon` | — | `200 {}`；該 Turn 既非 in-flight 也非 delivery=unknown → 409 |
| POST | `/api/bots/{id}/login` | — | `200 {"run_id":"...","kind":"claude","command":"/login"}`；見下 |

`keys` 可用的鍵名由 herdr 驗證，常用：`enter`、`esc`、`y`、`n`、`up`、`down`、`ctrl+c`。

**文字要用 `/text`，不要用 `/keys`。** `keys` 送的是鍵名，`\n` 不是任何一顆鍵的名字；一段
多行文字拆成鍵名會在半路被擋掉。`/text` 走 `pane.send_text`（herdr 眼中的貼上，換行原樣
留著），`enter`（預設 `true`）之後**另外**送一個 `enter` 鍵才是送出——同
`/api/hosts/{name}/shells/{pane_id}/text`。這條不看 `agent_status`：它的用途正是回合跑到一半
時再補一句（前端的「併送」）。

### 4.1 登入 / 切換帳號

`POST /api/bots/{id}/login` 把登入用的 slash 指令打進**正在跑的** bot 自己的 TUI，讓它進入
登入流程。daemon 只負責送指令：agent 接著會停在登入畫面（通常會開瀏覽器），**在使用者完成
之前那個 bot 不能工作**；登入完成與否由 `POST /api/hosts/{name}/tools/refresh` 重新偵測。

各 kind 的指令（實測 CLI 的 slash 補完選單，非推測）：

| kind | 指令 | 出處 |
|---|---|---|
| `claude` | `/login` | claude 2.1.263 選單：「Sign in with your Anthropic account」 |
| `grok` | `/login` | grok 1.0.13 選單：「Log in or re-authenticate with your account」；另見 `~/.grok/docs/user-guide/04-slash-commands.md` §Account and Billing |
| `codex` | **無** | codex 0.153.4 的選單只有 `/logout`，登入要在 TUI 外面跑 `codex login` |

錯誤（`error` / `reason` 是穩定的機器 key，文案由前端翻）：

| 狀況 | 回應 |
|---|---|
| 沒有這個 bot | `404 {"error":"not_found","what":"bot"}` |
| 這個 kind 的 TUI 沒有登入指令 | `400 {"error":"login_unsupported","kind":"codex","message":"..."}` |
| 沒在跑 / run 不是 `running` | `409 {"error":"conflict","reason":"not_running",...}` |
| agent 正在忙（`working` / `blocked`） | `409 {"error":"conflict","reason":"agent_busy",...}` |
| 有回合進行中（會被吃成 prompt 的一部分） | `409 {"error":"conflict","reason":"turn_in_flight",...}` |
| run 沒有 pane 可以打字 | `409 {"error":"conflict","reason":"no_pane",...}` |
| herdr 拒絕 | `502 {"error":"upstream","message":"..."}` |

### 4.2 強制中止 `POST /api/bots/{id}/abort`

`interrupt` 的語義是「請 agent 停下來」：`esc` 送不出去（pane 沒了、herdr 斷線、run 已經不在）
就整個失敗，那一回合仍掛在 `in_flight`，輸入框跟著鎖死，使用者只剩「停掉整個 bot」。

`abort` 反過來——**先保證解鎖**，送鍵只是順帶：

- `esc` 盡力送一次，成功與否寫在 `keys_sent`（失敗時 `key_error` 帶原因），**不影響**其餘步驟。
- in-flight 的回合標成 `failed`，並在對話裡留一則系統訊息「回合已由使用者強制中止」。
- 同一個 bot 底下 `delivery = "unknown"` 的回合一併收掉（改成 `failed`）——它同樣會擋住下一則
  prompt（§5），使用者要的是「現在就能再打字」，不是分兩顆按鈕點兩次。
- **沒有 active run 不是錯誤**：run 已經沒了、回合卻還掛著，正是最需要這支的情況。
- bot 不存在 → `404`。回合本來就沒有卡住 → `200 {"aborted":[]}`（冪等）。

註：agent 那頭可能還在跑（`esc` 沒送成功時），daemon 只是不再等它；真的要停就 `stop`。

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
      "group_id": null,
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


## GET /api/fs/dirs?path=&hidden=

目錄瀏覽（新增 Project 的目錄選擇器用）。`path` 省略或空白時為家目錄；支援 `~` 前綴。只列子目錄；symlink 指向目錄者也列出。
`.` 開頭的隱藏目錄預設略過，帶 `hidden=1`（或 `true` / `yes`）才會一起列出——選擇器的「隱藏資料夾」勾選框就是打這個參數（本機與 `host=` 遠端行為一致）。

```json
{"path":"/Users/me/project","parent":"/Users/me","home":"/Users/me",
 "entries":[{"name":"foo","path":"/Users/me/project/foo","git":true}]}
```

錯誤：路徑不存在或不是目錄 → 400 `{"error":"bad_request","message":"..."}`。


## bot.auto_approve（2026-09-06 新增）

每個 bot 的布林欄位，預設 `true`。啟動時 daemon 依 kind 注入略過權限確認的旗標：claude `--dangerously-skip-permissions`、codex `--yolo`（等同 `--dangerously-bypass-approvals-and-sandbox`）、grok `--always-approve`（等同 `--permission-mode bypassPermissions`）。`POST /projects/:id/bots` 與 `PATCH /bots/:id` 皆接受 `auto_approve`。舊資料庫啟動時自動 `ALTER TABLE` 補欄位。


## GET /api/mem（SPEC §15，2026-09-06 新增）

herdr 這一側現在佔多少常駐記憶體。左上角那一格用的就是這支。

**量的是整棵 herdr 進程樹**：`herdr` 本身 ＋ 它底下的 pane 與 agent CLI。錢是花在 pane 裡那個
`claude` 上，只報 herdr daemon 自己沒有意義。判定方式是「執行檔名等於 `herdr`」的 process 當
樹根（`grep herdr` 這種 argv 裡剛好有這個字的不算），再把子孫全部加總；herdr 底下再開 herdr
只算一次。

```json
{"total_bytes":1610612736,"herdr_bytes":50331648,"agents_bytes":1560281088,"processes":5,
 "hosts":[{"host":"local","herdr_bytes":50331648,"agents_bytes":1560281088,
           "total_bytes":1610612736,"processes":5,"error":null},
          {"host":"m4p","herdr_bytes":0,"agents_bytes":0,"total_bytes":0,"processes":0,
           "error":"未連線"}]}
```

- 每台主機一次 `ps -Awwo pid=,ppid=,rss=,args=`（遠端走既有的 ssh master），RSS 由 KiB 換成 bytes。
- **量不到的主機用 `error` 回報，不會從清單消失**：總和悄悄變小比沒有數字更糟。UI 會在數字旁
  標星號。
- daemon 每 15 秒取樣一次，**變化超過 1 MiB 才**推 WS `mem_updated`（frame 與這支同形狀）；
  這支端點任何時候都可以直接問。

舊 daemon 沒有這支 → 前端拿不到就整格不顯示。


## GET /api/mem/processes（SPEC §15.2，2026-09-08 新增）

`GET /api/mem/processes?host=local`（`host` 省略 = `local`；不認得的主機回 404）。
那一格的數字是由哪些程序組成的，以及**哪些是使用者自己開的、可以砍**。

```json
{"host":"local","sampled_at":"2026-09-08T04:11:02Z","processes":[
  {"pid":59407,"ppid":37845,"rss_bytes":412000000,"exe":"claude",
   "argv":"claude --dangerously-skip-permissions",
   "pane_id":"w168:p1","bot_id":"b3","bot_name":"opus","project_id":"p1",
   "owner":"bot","subtree_bytes":420000000,"children":3}]}
```

- `owner`：`bot`（環境有 `AM_BOT_ID`）、`pane`（只有 `HERDR_PANE_ID`，使用者自己開的）、
  `herdr`（herdr 本身，不會出現在清單裡）、`unknown`（兩個都讀不到）。判定規則見 SPEC §15.2。
- `bot_id` 查得到就補 `bot_name` / `project_id`；bot 已刪仍回 `bot_id`、`owner` 仍是 `bot`。
- `subtree_bytes` = 自己 ＋ 所有子孫的 RSS，也就是「砍掉這個能省多少」；清單依它降冪。
- 只列 `claude`/`codex`/`grok`/`node`/`bash`/`zsh`/`sh`/`fish` 且 `subtree_bytes ≥ 8 MiB` 的，
  其餘併進父程序的 `subtree_bytes`。
- 一次取樣同時拿樹與環境（macOS `ps -Ewwo`、Linux `/proc/<pid>/environ`），遠端走 ssh。

## POST /api/mem/processes/kill（SPEC §15.2，2026-09-08 新增）

```json
{"host":"local","pid":59407,"signal":"TERM"}
```

`signal` 省略 = `TERM`，只認 `TERM` / `KILL`。成功回
`{"host":"local","pid":59407,"signal":"TERM","exe":"claude","freed_bytes":420000000}`，
並立刻取樣推一次 `mem_updated`。

**送訊號前一定重新取樣再判定**（pid 會被回收）：

| 情況 | 回應 |
|---|---|
| pid 不在該主機的 herdr 樹裡 | 400 `pid … 不在 … 的 herdr 樹裡` |
| 該 pid 就是 `herdr` | 400 `不能砍 herdr 本身` |
| `owner == "bot"` | 409 `{"error":"conflict","reason":"bot_process","bot_id":"b3","message":"這是 AG Man 的 bot，請用停止 bot"}` |

砍 bot 走既有的 `POST /bots/{id}/stop`，那條路才會記錄停止。


## GET /api/mem/processes/pane（SPEC §15.2，2026-09-08 新增）

`?host=local&pane_id=wM:pB&socket=/Users/me/.config/herdr/herdr.sock&lines=40`（`lines` 1–500，預設 40）。
`socket` 是清單那列的 `socket_path`：**pane id 是 per herdr session 的**，使用者自己開的 pane 多半在 `default`
session 而不是 `agents-manager`，daemon 就直接連那個 socket 讀（只限本機；遠端給了 `socket` 回 400）。
省略 `socket` 時走該主機設定的 session。回那個 pane 現在畫面上的字，
形狀同 `GET /api/hosts/{name}/shells/{pane_id}/terminal`，但 `source` 固定 `visible`，而且
**不要求那個 pane 是 AG Man 開的**——清單裡的 pane 正是使用者自己開的。只讀不寫；沒有對應的
text / keys 端點。

```json
{ "host":"local","pane_id":"wM:pB","source":"visible","text":"…","revision":12,"truncated":false,"columns":185,"rows":54 }
```

主機沒接上 herdr → 502；`pane_id` 缺 → 400；pane 不存在 → herdr 的錯誤照回（502）。


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
     "remote_path":null,"connected":true,"error":null},
    {"name":"m4p","ssh":"m4p@100.112.229.82","ssh_port":22,"ssh_opts":[],
     "herdr_session":"agents-manager","remote_path":"/opt/homebrew/bin:$HOME/.local/bin",
     "connected":false,"error":"ssh master exited immediately (exit status: 255)"}
  ],
  "projects": [
    {"id":"01M1...","path":"/Users/m4p/work/foo","label":"foo@m4p","host":"m4p",
     "workspace_id":null,"bots":[ ... ]}
  ]
}
```

- `hosts` **必定含 `local`**，且 `local` 永遠排在第一個；`local` 的 `ssh` / `ssh_port` /
  `remote_path` 為 `null`，`connected` = 本機 herdr 連線狀態（與頂層 `connected` 同值）。
- `default_connected` 是本機使用者 Herdr `default` session 的觀察連線狀態；採用該 session 的 Bot
  只依這個欄位判斷，不會因 manager 的 named session 狀態誤亮或誤灰。
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
| `hook_port` | | v4.3 起**忽略**（仍接受，只為相容舊 client）。遠端 hook 改走該機器自己的 herdr 上報狀態、payload 寫進 spool 檔由 daemon 讀回，沒有反向轉發也沒有埠可挑，見 SPEC §11.4 |
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

### `GET /api/hosts/{name}/gh`（2026-09-07）

該主機上的 GitHub CLI 登入狀態。issue 列表與 team 都靠這台上的 `gh`，遠端主機沒登入（或作用中帳號的 token 失效）時，`GET /projects/{id}/issues` 會 502。`name = local` 是本機。

```json
{
  "name": "m4p",
  "installed": true,
  "path": "/opt/homebrew/bin/gh",
  "logged_in": false,
  "account": "eddysun-alt",
  "accounts": [
    {"login": "eddysun-alt", "active": true, "ok": false},
    {"login": "Eden-Sun", "active": false, "ok": true}
  ],
  "mode": null,
  "pending": null,
  "error": null
}
```

- `logged_in`：**作用中**帳號的 `gh auth status --json` 為 `success` 才是 `true`。有帳號但 token 401 仍是 `false`。
- `pending`：進行中的裝置碼（見下一支），不含 `device_code`、不含 token。
- host 不存在 → 404；ssh 失敗 → 502。`gh` 沒裝時 `installed: false`、`logged_in: false`（仍 200）。

### `POST /api/hosts/{name}/gh/login`

在 UI 裡對該主機做 `gh` 登入，不必 ssh 過去。body：

```json
{ "mode": "auto", "user": null }
```

`mode` 可省，預設 `auto`。`user` 只在 `switch` 時用。

| mode | 行為 |
|---|---|
| `auto` | 已可用 → 原樣回。否則若有有效但非 active 的帳號 → `switch`。遠端且本機已登入 → `copy`。其餘 → `device`。 |
| `switch` | `gh auth switch --hostname github.com --user <user>`。沒給 `user` 就切到第一個有效的非 active 帳號。 |
| `copy` | 本機 `gh auth token` 經 ssh **stdin** 餵給遠端 `gh auth login --with-token --insecure-storage`（token 不進 argv、不進 log）。只適用遠端。 |
| `device` | daemon 向 GitHub 要裝置碼，立刻回 `pending`；背景輪詢，授權後同樣 `--with-token` 餵給該主機。 |

回應形狀同 GET，外加實際走的 `mode`。`device` 當下：

```json
{
  "name": "m4p",
  "installed": true,
  "logged_in": false,
  "mode": "device",
  "pending": {
    "user_code": "WDJB-MJHT",
    "verification_uri": "https://github.com/login/device",
    "verification_uri_complete": "https://github.com/login/device?user_code=WDJB-MJHT",
    "expires_in": 899
  }
}
```

之後前端每 ~2 秒 `GET …/gh`：`logged_in: true` 即完成；`error` 有字則失敗（過期 / 拒絕）。token、`device_code` 絕不出現在 JSON 或 log。

| 狀況 | 回應 |
|---|---|
| host 不存在 | `404 {"error":"not_found","what":"host"}` |
| `mode` 不合法 | `400` |
| `copy` 打在 `local` | `400` |
| `copy` 但本機 gh 未登入 | `409 {"error":"conflict","reason":"local_gh_not_logged_in"}` |
| 沒有可切的帳號 | `400` |
| ssh / gh / GitHub API 失敗 | `502 {"error":"upstream","message":"…"}`（message 已打碼，不含 token） |

### `POST /api/hosts/{name}/gh/cancel`

放棄進行中的裝置碼。回應同 GET（`mode: "cancel"`，`pending: null`）。

## 主機 shell（2026-09-07 新增）

在某台主機（`local` 或設定過的 `[[hosts]]`）開一個**純 shell** 的 herdr pane，用來裝工具、看 log、跑 `gh auth status`、清 worktree 這類雜事。沒有 agent、沒有 run、沒有 turn，也不發任何 WebSocket 事件——狀態就是那張終端快照，由呼叫端自己輪詢。

daemon 只認**它自己開的** pane：每一支端點（`POST …/shells` 以外）都先在記憶體的清單裡找 `(host, pane_id)`，找不到就 404 `{"error":"not_found","what":"shell"}`。這張清單不落地，所以 daemon 重啟後所有舊 pane 一律不認（也就不可能拿 bot 的 pane_id 來送鍵）。

pane 開在該主機**manager 的那個 session**（`[server] herdr_session` / `hosts[].herdr_session`）。本機的 `default` session 是使用者自己的，daemon 不會往裡面開東西。

### `POST /api/hosts/{name}/shells`

```json
{ "cwd": "/Users/m4p/work/foo" }
```

`cwd` 可省（body 也可以整個省）：省略時取該主機任一 live project 的 `path`，都沒有就用該主機的 `$HOME`。

```json
{
  "host": "m4p",
  "pane_id": "wPQ:p1",
  "tab_id": "wPQ:t1",
  "workspace_id": "wPQ",
  "cwd": "/Users/m4p",
  "herdr_session": "agents-manager",
  "created_at": "2026-09-07T03:19:40.911Z"
}
```

- workspace 先**借**該主機某個 project 的（用 `workspace.get` 確認還在），借到就 `tab.create` 開一個新分頁；都借不到才 `workspace.create`（標籤 `shell`）並直接用它的 root pane。新建的 workspace **不會**寫回 `projects.workspace_id`。
- `cwd` 回的是 herdr **實際**開起來的目錄，不一定等於送進來的那個。
- 每台主機最多 8 個：超過 → `409 {"error":"conflict","reason":"too_many_shells","host":"…","max":8}`。
- host 不存在 → 404；主機沒連線 → `502 {"error":"upstream","message":"host \`m4p\` is not connected"}`。

### `GET /api/hosts/{name}/shells`

```json
{ "host": "m4p", "max": 8, "shells": [ { …同上… } ] }
```

回之前會對每一列打 `pane.get`，pane 已經不在（使用者在 herdr 裡自己關掉）就從清單移除再回。herdr 問不到（不是「不在」）時保留該列，交給下一次輪詢——問不到不等於死掉。

### `GET /api/hosts/{name}/shells/{pane_id}/terminal?source=visible&lines=200`

形狀比照 §7，只是主體是 host / pane 而不是 bot / run：

```json
{
  "host": "m4p", "pane_id": "wPQ:p1", "cwd": "/Users/m4p",
  "source": "visible", "text": "……純文字，已去 ANSI……",
  "revision": 0, "truncated": false,
  "columns": 185, "rows": 54
}
```

`source ∈ visible | recent | recent_unwrapped | detection`（預設 `visible`），`lines` 1–2000（預設 200）。

⚠️ `recent` / `recent_unwrapped` 只給**已經捲出畫面**的內容：還沒捲過的 pane 兩者都回 `text: ""` + `truncated: true`，而 `visible` 是有內容的（實測 2026-09-07，herdr 0.8.2）。前端要把這件事說出來，不要顯示一片空白。

### `POST /api/hosts/{name}/shells/{pane_id}/text`

```json
{ "text": "echo hi", "enter": true }
```

`enter` 可省（預設 `true`）。Enter 是獨立的一次 `pane.send_keys(["enter"])`，**不是**文字裡的 `\n`——對 herdr 來說換行是「貼上」而不是「按下 Enter」。`text` 允許空字串：`{"text":"","enter":true}` 就是「只按 Enter」。回 `200 {}`。

### `POST /api/hosts/{name}/shells/{pane_id}/keys`

```json
{ "keys": ["ctrl+c"] }
```

鍵名原樣送 herdr `pane.send_keys`，daemon 不翻譯（同 `POST /bots/{id}/keys`：`enter` / `esc` / `tab` / `up` / `down` / 單一字元 / `ctrl+c` 這種疊修飾詞）。空陣列 → 400。回 `200 {}`。

### `DELETE /api/hosts/{name}/shells/{pane_id}`

`pane.close`，若這樣讓分頁空了就連分頁一起收。**冪等**：清單裡沒有那一列也回 `200 {}`（已經沒了就是成功）。只有連主機都解析不出來才是錯誤。

### `POST /api/projects` 新增 `host`

```json
{ "path": "/Users/m4p/work/foo", "label": "foo@m4p", "host": "m4p" }
```

`host` 可省（預設 `"local"`）。指定未設定的 host → `404 {"error":"not_found","what":"host"}`。
**遠端 project 的 `path` 不做本機 canonicalize**（本機不存在該路徑）：daemon 會透過 ssh 檢查該目錄
存在並取得遠端 canonical path，失敗 → `400`。

### `GET /api/fs/dirs?host=<name>&path=<path>&hidden=`

`host` 可省（預設 `local`）。指定 host 時，daemon 透過 ssh 列出**遠端**目錄，回傳格式與本機完全相同（`hidden=1` 同樣會把 `.` 開頭的目錄一起列出）：

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


## 身份 identities（2026-09-06 新增）

同一種 agent 想用不同帳號執行時，用 **identity**：一組具名的 env + args，套在 bot 上。
典型用法是 claude 的 `CLAUDE_CONFIG_DIR`（等同使用者原本的 `cc0` / `cc1` alias）：

```toml
[[identities]]
name = "cc1"                                          # [a-z][a-z0-9_-]{0,31}，唯一
kind = "claude"                                       # claude | codex | grok
args = []
[identities.env]
CLAUDE_CONFIG_DIR = "$HOME/.claude-ccompany"

  [[projects.bots]]
  name = "foo-cc1"
  kind = "claude"
  identity = "cc1"
  [projects.bots.env]                                 # bot 自己的 env（覆蓋 identity）
  FOO = "bar"
```

啟動 Run 時：

- **pane env** = daemon 既有注入（`AM_BOT_ID` / `AM_RUN_ID` / `AM_PORT`（v4.3 起只有本機 bot 帶，遠端 hook 不打 HTTP） /
  `CLAUDE_CODE_CHILD_SESSION` / `CLAUDECODE`）∪ `identity.env` ∪ `bot.env`，後者覆蓋前者。
- **args** = daemon 注入（`--dangerously-skip-permissions` / `--settings` …）
  ++ `identity.args` ++ `bot.args`。
- env 值中的 `$HOME`、`${HOME}` 與**開頭**的 `~` 會展開成**該 host 的 home**
  （本機用本機 home，遠端用 ssh `echo $HOME` 取得並快取）。
- identity 的 `kind` 與 bot 的 `kind` 不符 → `400`。

### `GET /api/state` 新增欄位

```json
{
  "identities": [
    {"name":"cc0","kind":"claude","env":{},"args":[]},
    {"name":"cc1","kind":"claude","env":{"CLAUDE_CONFIG_DIR":"$HOME/.claude-ccompany"},"args":[]}
  ],
  "projects": [
    {"bots": [
      {"id":"01M1…","name":"foo-cc1","kind":"claude","identity":"cc1","env":{"FOO":"bar"}, "…": "…"}
    ]}
  ]
}
```

- `identities` 一定存在（沒設定時為 `[]`）。**這是 config.toml 的那一份**；每台主機另外還有從它自己
  登入 shell 認出來的 `ccN`（見下方「shell 認出來的身份」）。
- 每個 bot 物件都有 `identity`（`string | null`）與 `env`（物件，預設 `{}`）。

### shell 認出來的身份 `ccN`（v4.1，SPEC §16）

daemon 在每台主機的工具偵測裡順便讀那台登入 shell 的 alias（`"$SHELL" -lic alias`，讀不到時退回
`~/.zshrc`），把 `cc0`…`cc6` 當成身份用，**不寫回 config.toml**：

```json
{
  "name": "m4p", "…": "…",
  "shell_identities": [
    {"name": "cc0", "kind": "claude", "env": {}, "args": []},
    {"name": "cc1", "kind": "claude", "env": {"CLAUDE_CONFIG_DIR": "$HOME/.claude-ccompany"}, "args": []}
  ],
  "identities": {
    "cc0": {"name":"cc0","kind":"claude","logged_in":true,"account":"me@example.com","plan":"max","source":"config"},
    "cc1": {"name":"cc1","kind":"claude","logged_in":true,"account":"ops@example.com","source":"shell",
            "config_dir":"/Users/m4p/.claude-ccompany"}
  }
}
```

- `shell_identities`：那台主機讀到的原始 `ccN`（`env` 未展開，就是 alias 裡的字面值）。
- `identities.<name>.source`：`config`（config.toml 的 `[[identities]]`，可編輯 / 可刪）或 `shell`
  （alias 認來的，唯讀）。舊 daemon 沒有這個欄位 → 一律當 `config`。
- `identities.<name>.config_dir`：該身份在**這台**指到的設定目錄，`$HOME` 已用那台的家目錄展開；
  `cc0` 這種預設帳號沒有這個欄位。
- 只取 alias 開頭的 `CLAUDE_CONFIG_DIR=`；alias 裡的旗標（`--dangerously-skip-permissions` 等）
  **不會**被帶進來，授權旗標仍由 daemon 的 `auto_approve` 決定，所以 `args` 一定是 `[]`。
- 同名時 config 的那一個勝出（`tools::identities_for_host`），bot 啟動、identity 驗證、額度探測與 UI
  選單走的都是這條合併規則。
- `identity` 的存在性是**在該 bot / 專案的 host 上**檢查的：本機有 `cc2`、遠端沒有時，把遠端 bot 設成
  `cc2` 會拿到 `404 {"error":"not_found","what":"identity"}`。

### bot 建立 / 修改

`POST /api/projects/{id}/bots` 與 `PATCH /api/bots/{id}` 都多接受兩個欄位：

```json
{ "name":"foo-cc1", "kind":"claude", "identity":"cc1", "env":{"FOO":"bar"} }
```

- `identity` 省略 = 不變（PATCH）／`null`（POST）；傳 `null` 或 `""` 可解除綁定。
- `env` 省略 = 不變（PATCH）／`{}`（POST）；傳整個物件會**取代**既有的 env。
- 指定不存在的 identity → `404 {"error":"not_found","what":"identity"}`（存在與否看的是那個 bot 所在
  主機的清單：config.toml 的 `[[identities]]` ∪ 那台 shell 的 `ccN`）。
- identity 的 kind 與 bot kind 不符 → `400`。

### `POST /api/identities`

```json
{ "name":"cc1", "kind":"claude", "env":{"CLAUDE_CONFIG_DIR":"$HOME/.claude-ccompany"}, "args":[] }
```

`200 {"name":"cc1"}`；名稱不合 `[a-z][a-z0-9_-]{0,31}` 或 `kind` 不是 claude/codex/grok → `400`；
名稱重複 → `409 {"error":"conflict","reason":"identity name already in use","name":"cc1"}`。

### `DELETE /api/identities/{name}`

`200 {}`；仍有 bot 綁著 → `409 {"error":"conflict","reason":"identity still used by bots","bot_id":"…"}`。

### WebSocket

| type | data |
|---|---|
| `identities_changed` | `{}` — 重新 `GET /api/state` |

bot 的 `identity` / `env` 變更沿用既有的 `bot_changed`。

---

## 10. Bot 編輯 / 刪除 / 指定模型（v3.3，2026-09-06 新增）

### 10.1 `bot.model`

每個 bot 新增可選欄位 **`model`**（`string | null`，預設 `null` = 不指定，由 CLI 自己決定）。
啟動 Run 時 daemon 依 kind 注入：

| kind | 注入 |
|---|---|
| `claude` | `--model <model>` |
| `codex` | `-m <model>` |
| `grok` | `-m <model>`（`grok models`：`grok-4.6` 預設、`grok-4.5`） |

**argv 組合順序**（前端可據此預覽）：

```
daemon 旗標（auto_approve: --dangerously-skip-permissions / --yolo；hooks: --settings / -c notify=…）
  → model（--model <m> / -m <m>）
  → identity.args
  → bot.args
```

- TOML：`model = "opus"`（`[[projects.bots]]` 內）。
- DB：`bots.model TEXT`（additive migration，舊 DB 啟動時自動補欄位）。
- `GET /api/state` 的每個 bot 物件都有 `model`（`string | null`）。
- `POST /api/projects/{id}/bots` 可帶 `model`（省略 = `null`）。
- 值不做白名單驗證（各 CLI 自己驗），只把空白字串正規化成 `null`。

### 10.2 `PATCH /api/bots/{id}`

body（所有欄位皆可省略；`model` 與 `identity` 可傳 `null` 清除）：

```json
{
  "name": "am-codex",
  "model": "gpt-5.5",
  "args": ["--search"],
  "autostart": false,
  "auto_approve": true,
  "inject_hooks": true,
  "identity": "cc1",
  "env": {"FOO": "bar"}
}
```

回應：

```json
200 {"needs_restart": true}
```

- **`needs_restart`**：`true` 表示這次修改要等 bot 重啟後才會生效（有 active Run，且本次動到會影響啟動 argv / env 的欄位：`model`、`args`、`identity`、`env`、`auto_approve`、`inject_hooks`）。
  - 例外（TUI slash 指令當場套用）：**只**改該欄位、Run 在 `running` 且不忙（非 working / blocked、沒有 in-flight turn）、新值不是清成 `null` 時，daemon 會往 pane 送對應的 slash 指令（`pane.send_text` → 0.8s → `Enter`）並回 `needs_restart: false`。任何一個條件不成立就退回 `true`。
    - grok `effort` → `/effort <level>`
    - grok `model` → `/model <id>`；若這次 PATCH 也帶了 `effort`（或 bot 本來就有），第二參數一併送（`/model grok-4.6 high`）
    - claude `model` → `/model <alias>`（alias 同 `claude --model`：`opus` / `sonnet` / `haiku` / `fable`）
    - claude `effort` → `/effort <level>`（2.1.263 實測：帶參數就直接套用；不帶參數的 `/effort` 才是拉桿）。
      **副作用**：claude 會把它一併存成該帳號之後新 session 的預設強度（CLI 行為，TUI 上按 `s` 才是只此一次）
  沒有 active Run，或只改 `autostart`（下次啟動才用得到）→ `false`。
  前端可據此顯示「需要重新啟動」並提供 §10.3 的按鈕。
- **`name`**：有 active Run 時 **409**（herdr agent name 綁在啟動時的名稱上）：

  ```json
  409 {"error":"conflict","reason":"cannot rename a bot with an active run","run_id":"01M1…","bot_id":"01M1…"}
  ```

  停掉 bot 之後即可改名。名稱不合 `[a-z][a-z0-9_-]{0,31}` → 400；與其他 bot 重名 → 409 `{"reason":"bot name already in use","name":"…"}`。
- **其他欄位在有 active Run 時允許修改**（不再 409），只是回 `needs_restart: true`。
- `identity` 指向不存在的 identity → `404 {"error":"not_found","what":"identity"}`；kind 與 bot 不符 → `400`。
- `env` 傳整個物件即為**取代**（不 merge）；`identity` / `model` 傳 `null` 或 `""` 即為清除。
- 成功後推 WS `bot_changed {bot_id}`。

### 10.3 `POST /api/bots/{id}/restart`

等同「有 Run 就先 stop（§6.4：ctrl+c ×2、逾時關 pane）→ 再 start」，用來讓改過的 `model` / `args` /
`identity` / `env` 生效。

```json
200 {"run_id":"01M1…"}        // 新 Run 的 id
```

- 本來就沒有 Run 也可以呼叫，等同 start。
- start 失敗的錯誤與 `POST /bots/{id}/start` 相同（502 / 409 / 404）。
- 過程中會推 `bot_status`（stopping → offline → starting → idle）。

### 10.4 `DELETE /api/bots/{id}`

```json
200 {}
```

流程：有 active Run 先 stop（ctrl+c ×2、逾時關 pane；遠端 host 亦同）→ 從 config.toml 移除 →
DB `bots.deleted_at`（**Conversation 與所有訊息保留**，同一個 bot id 之後仍查得到歷史）→
刪除該 bot 的 hook 材料目錄 `~/.config/agents-manager/bots/<bot_id>/`（遠端 host 以 ssh `rm -rf`，
失敗只寫 log、不影響回應）。

- 找不到 bot → `404`。
- 刪除後推 `bot_changed {bot_id}`；前端重新 `GET /api/state`（該 bot 會從 `projects[].bots` 消失）。
- 訊息歷史仍可用 `GET /api/bots/{id}/messages` 讀到（第一階段不提供「已刪除 bot」的列表 UI）。

### 10.5 WebSocket

沿用既有事件，沒有新型別：

| type | data |
|---|---|
| `bot_changed` | `{"bot_id":"…"}` — PATCH / DELETE / restart 都推這個，收到後重新 `GET /api/state` |


---

## 11. 專案群組聊天（SPEC §13，v3.6，2026-09-06 新增）

一個 Project 就是一個群組。使用者在群組裡輸入 `@<bot 名稱>` 或 `@all`，daemon 把同一段文字
（保留原文，含 `@`）以 §5 的 prompt 路徑送給每個目標 bot；各 bot 的回覆回到同一條合併時間軸。
**沒有新的 conversation 型別**，也不會自動啟動 bot。

### 11.1 `messages.group_id`

每個 message 物件多一個欄位 **`group_id`**（`string | null`）：同一次 `POST /projects/:id/chat`
產生的每個收件 bot 的 user 副本、以及「未送達」的 system 註記共用同一個 `group_id`
（= 該次的 `client_request_id`）。bot 的回覆與其他訊息為 `null`。
`GET /bots/:id/messages` 與 WS `message_added` 的 message 物件都帶這個欄位。

### 11.2 `GET /api/projects/{id}/messages?before=<message_id>&limit=100`

Project 底下**所有存活 bot** 的訊息合併，以 `message.id`（ULID，時間有序）倒序分頁，
回傳時已**正序**排好。`limit` 1–500（預設 100）。Project 不存在 → 404。

```json
{
  "project_id": "01M1...",
  "messages": [
    {
      "id": "01M1...", "conversation_id": "01M1...", "turn_id": "01M1...",
      "role": "user", "content": "@all Reply with exactly GROUP-OK",
      "source": "web", "incomplete": 0, "terminal_snapshot": null,
      "group_id": "c-group-1",
      "created_at": "2026-09-05T18:12:30.121Z", "updated_at": null,
      "bot_id": "01M1...", "bot_name": "g-claude"
    },
    { "...": "同一則 user 訊息在 g-codex 上的副本（group_id 相同）", "bot_name": "g-codex" },
    { "role": "assistant", "content": "GROUP-OK", "source": "hook", "group_id": null, "bot_name": "g-claude", "...": "…" }
  ],
  "has_more": false
}
```

前端把同一 `group_id` 的 user 訊息折疊成一則並列出目標 bot。

### 11.3 `POST /api/projects/{id}/chat`

```json
{ "text": "@all Reply with exactly GROUP-OK", "client_request_id": "<前端產生的唯一字串>" }
```

- **mention 規則（daemon 為準）**：`@all` = 專案內所有 bot；`@<name>` 比對專案內 bot 名稱，
  大小寫不敏感，token 為 `[A-Za-z0-9_-]+`（`@name,` / `@name:` 的尾隨標點會被切掉；
  `@name-` / `@name_` 整個對不上時去掉尾隨 `-` `_` 再試）；`@` 必須在開頭或接在非字元後
  （`me@example.com` 不算）。目標依專案內 bot 順序去重。
- `client_request_id` 同時是 **`group_id`**；每個收件 bot 的 prompt 用 `<crid>:<bot_id>` 做冪等鍵，
  所以同一 id 重送會回同一組 `turn_id`，也不會再寫第二則「未送達」註記。

成功 `200`（部分 bot 略過仍是 200）：

```json
{
  "group_id": "c-group-4",
  "project_id": "01M1...",
  "sent": [
    { "bot_id": "01M1...", "bot_name": "g-claude", "turn_id": "01M1...", "message_id": "01M1...", "delivery": "ok" }
  ],
  "skipped": [
    { "bot_id": "01M1...", "bot_name": "g-codex", "reason": "not_running", "detail": "bot has no active run" }
  ]
}
```

- `sent[].delivery` 意義同 §5（`ok` / `unknown` / `failed`）。
- `skipped[].reason`：`not_running`（無 active Run 或 Run 非 running）、`blocked`、`in_flight`、
  `unknown_delivery`、`conflict`、`not_found`、`bad_request`、`upstream`；`detail` 為人類可讀原因
  （即單一 bot prompt 會回的 409 `reason`）。每個略過的 bot 的 conversation 會多一則
  `role=system`、`group_id` 相同的訊息（例：「群組訊息未送達 g-codex：bot 未啟動（不會自動啟動）」），
  並經 `message_added` 推送。**不會自動啟動 bot。**

錯誤：

| 狀態碼 | body |
|---|---|
| 400 | `{"error":"no_mention","message":"…","bots":[{"id":"…","name":"g-claude","kind":"claude"}]}` — 沒有任何有效 mention；`bots` 列出可用名稱 |
| 400 | `{"error":"bad_request","message":"text must not be empty"}` |
| 404 | `{"error":"not_found","what":"project"}` |

### 11.4 WebSocket

沒有新事件。每個收件 bot 各自推 `message_added`（user 副本，帶 `group_id`）與 `turn_updated`；
回覆到達時推該 bot 的 `message_added`。前端依 `bot_id → project_id` 歸入群組時間軸。

### 11.5 daemon 的 `AM_DATA_DIR`

`agents-managerd serve` 讀環境變數 `AM_DATA_DIR` 覆蓋資料目錄（預設 `~/.config/agents-manager`；
`hook` 子命令早已支援同名變數）。用途：在不動正式 daemon 的情況下起第二個實例驗證，例如
`AM_DATA_DIR=/tmp/am-group agents-managerd serve --config /tmp/am-group/config.toml`
（config 用另一個 `listen` port 與 `herdr_session`）。
## bot.agent_name（v3.5）

`GET /api/state` 的 bot 物件新增唯讀欄位 `agent_name`：herdr 內的 agent 名稱。有 active Run 時為該 run 實際啟動的名稱；否則為下次啟動會用的 `<project label slug>-<bot name>`（例如 `agents-manager-am-codex`）。bot `name` 的唯一性改為**專案內**唯一（同名 bot 可存在於不同專案）；`POST /projects/:id/bots` 與 `PATCH /bots/:id` 的重名 409 訊息改為 `bot name already in use in this project`。


## bot.kind = "grok"（v3.6，SPEC §12）

第三種 kind：xAI grok CLI（1.0.13）。`POST /projects/:id/bots`、`POST /identities` 的 `kind` 接受 `claude | codex | grok`（其他值 → `400 {"error":"bad_request","message":"kind must be claude, codex or grok"}`）。前端與 mock 都已有 `grok` 選項與 `am-grok` 種子。

啟動時 daemon 注入：`auto_approve` → `--always-approve`；`model` → `-m <model>`；hook **不走 argv**（grok 沒有每次啟動的 hook 旗標），改為寫入 `<GROK_HOME>/hooks/agents-manager.json` + `~/.config/agents-manager/grok-hook.sh`，靠 pane env `AM_BOT_ID` / `AM_HOOK_TOKEN`（本機另有 `AM_PORT`）分派到正確的 bot；`inject_hooks = false` 時不給 `AM_HOOK_TOKEN`，hook 變成 no-op，回覆走 `terminal_fallback`。

hook 端點：`POST /hook/grok`（body 與 claude 相同，`payload` 為 grok 的 stdin JSON：`hookEventName: "stop"`、`sessionId`、`promptId`、`transcriptPath`、`lastAssistantMessage`、`reason: "end_turn"`、`stopHookActive`）。`reason ≠ end_turn`（session 結束時的觀察用 Stop）與 `session_end` 會被忽略；`session_start` 只回填 `runs.native_session_id`。

對前端可見的差異：`kind: "grok"`；`GET /api/state` 其餘欄位相同；`terminal_fallback` 訊息不再含 grok 的遙測 banner / 時戳 / 捲軸字元。


## v3.8：暱稱、hash 式 agent name、effort

- `bot.name` 是暱稱：1–32 字、不可含空白或 `@ , : ;`，允許 CJK；`PATCH /bots/:id {name}` 在 run 執行中也可改（回 `needs_restart:false`），herdr 不受影響。
- `bot.agent_name`（唯讀）= `<project slug>-<bot id 尾 6 碼>`，例如 `agents-manager-rbmyf7`。
- `bot.effort`（`POST /projects/:id/bots`、`PATCH /bots/:id` 皆可設；值依 kind，其他值 400）：
  - `claude`：`low|medium|high|xhigh|max|null` → `--effort <level>`（claude **2.1+**；v4.1 起支援）。
  - `grok`：`low|medium|high|xhigh|null` → `--reasoning-effort <level>`。
  - `codex`：`none|minimal|low|medium|high|xhigh|max|ultra|null` → `-c model_reasoning_effort="<level>"`。
- `@mention` 解析（daemon 與前端一致）：token 為連續非空白、非標點字元，支援 `@小幫手，看一下`；送給 bot 的文字會去掉 mention 與其後的 `, : ; ，：；、`。


## WS `turn_progress`（v3.9，即時輸出）

回合 `in_flight` 且 `delivery=ok` 時，daemon 每 0.7 秒讀一次 pane（`recent_unwrapped`），把 prompt 回音之後、去掉 TUI 雜訊與回覆標記的文字推成：
```json
{"type":"turn_progress","seq":123,"data":{"bot_id":"…","run_id":"…","turn_id":"…","text":"目前為止的部分回覆","activity":"Thinking… (12s · ↑ 1.2k tokens)","alert":"","revision":42}}
```

- `text`：目前為止的部分回覆（同最終訊息的清理規則）。
- `activity`（**選填**，v4.1 新增）：agent 目前的**活動狀態**——畫面上該回合最後一行 spinner／事件行去掉開頭字元後的內容，例如 `Boogieing… (3m 18s · ↓ 11.0k tokens)`、`Thought for 0.1s`。上限 120 字元（超過截斷並補 `…`），沒有就送空字串。它**只給前端在 `text` 還是空的時候顯示用**，是純文字（不要當 Markdown 渲染），**不會**寫進 DB、也**不會**進入最終訊息。

  辨識規則是兩條**取聯集**、以該回合畫面上**最後一行**命中者為準：
  1. **glyph 白名單**（快路徑）：claude / codex `✻ ✽ ✶ ✳ ✢ ·`，grok `◆`。
  2. **結構性後備**（`is_activity_shape`）：不管前面是什麼 glyph、甚至沒有 glyph，只要 trim 後長成 `<單字>… (…)` 且括號內含 `tokens` 或時間樣式（`12s` / `3m` / `1h`）就算活動行。

  ⚠️ **spinner 的動詞是隨機的**（`Thinking`、`Boogieing`、`Improvising`、`Puttering`、`Simmering`…），任何字面比對都是錯的；glyph 集合也會隨 CLI 版本增減，所以白名單只是快路徑，真正撐住的是第 2 條的形狀比對。

  括號內的秒數每次輪詢都在變，因此 `activity` 幾乎每 0.7 秒都不同、每次都想發幀——這是**預期且想要**的行為（前端計時會跟著跳）。發幀的頻率上限見下面的「發幀節流」。

- `alert`（**選填**，v4.2 新增）：畫面上的**重試／API 錯誤橫幅**，例如 claude 的 `API error · Retrying in 0s · attempt 1/10`、codex 的 `stream error: 503 upstream; retrying 2/5 in 1s`。沒有就送空字串。同樣是純文字、不寫 DB、不進最終訊息，上限 120 字元。

  **為什麼需要它**：CLI 在重試上游失敗時，回合仍然 `in_flight`、spinner 仍然在轉、`activity` 仍然正常，UI 看起來完全健康——實際上 agent 卡在那裡重試。`alert` 是唯一會說出這件事的訊號，前端把它畫成氣泡下方的警示列（`docs/screenshots/280-live-alert-light.png` / `281-…-dark.png`）。

  辨識同樣**只看形狀不看字面**：該回合畫面上最後一行「夠短（≤240 字元）」且**同時**滿足「說了 error／錯誤／overloaded」與「帶重試 token（retry／retrying／attempt／reconnect／retries／重試）」；或該行以 `API error` 開頭。兩個條件都要，是為了把 agent 自己在回覆裡談論錯誤的散文擋在外面。

### 發幀節流（v4.3）

**同一個 `run_id` 每秒最多 4 個 `turn_progress` frame。** 兩幀之間至少隔 250 毫秒；在這段窗內產生的幀
會被**合併**——daemon 只留最新的那一個，中間的狀態不會補送（`text` / `activity` 本來就是「目前為止」
的快照，不是增量，丟掉中間態不會漏內容）。窗一開就把手上最新的那個送出去；回合結束、poller 收工時，
還壓在手上的最後一幀會**無條件**補送，所以回覆的最後狀態不會卡住。

實務上 0.7 秒的輪詢間隔本來就低於這個上限，所以正常串流看不出差別；這條是**協定保證**，讓前端可以
依此估算負載（多個 agent 同時串流時，每個 bot 的上界是 4 frame/s），也讓輪詢間隔之後調快時不會
一次把幀數乘上去。前端另有自己的合併（同一個 bot 250 毫秒內只套用最後一幀，見 docs/FRONTEND.md）。

只在 `text`、`activity` 或 `alert` **任一**變化時推；回合結束（hook / 備援 / watchdog / stop）後停止。前端應顯示為該回合的「即時氣泡」，收到同 turn 的 `message_added`（assistant）或 `turn_updated` 非 in_flight 時移除。實測 claude 8 行清單：5 幀、每幀 0.7 秒、內容逐步增長。

> 為什麼要 `activity`：清理管線（`clean_screen` / `is_noise`）把 spinner 行、框線與狀態列整行濾掉，而 agent 在**純思考／跑工具**的階段畫面上就只剩這些。整頁被濾成空字串後 `text` 一直沒變化 → 一幀都不發 → 前端氣泡永遠停在「等待回覆（hook）…」。`activity` 是繞過清理的獨立旁路，讓進度透出而不污染回覆內容。


---

## 12. v4.0：模型清單、fast 模式、attach 指令、額度

### 12.1 `GET /api/models?kind=claude|codex|grok&host=<name>&identity=<name>`

列出某 host 上某 kind 可用的模型。`host` 省略 = `local`。daemon 端快取 **10 分鐘**（key =
host+kind+identity），`?refresh=1` 強制重抓。遠端 host 透過 ssh（`HostConn::ssh_exec_path`）
在遠端跑同樣的管線。

`identity`（**claude only**，v4.2）：claude 的 `default_effort` 不是模型內建的，是那個帳號
`settings.json` 目前的設定（見下表與 SPEC §17.1）；`identity` 決定讀哪個
`CLAUDE_CONFIG_DIR/settings.json`（走 `identities_for_host`，含 SPEC §16 那些 shell 認來的
`ccN`）。省略、或給一個不存在 / 非 claude 的名字，一律退回預設帳號的 `~/.claude/settings.json`
——不是錯誤，只是沒有那個身份專屬的提示。codex / grok 忽略這個參數。

```json
{
  "kind": "codex",
  "host": "local",
  "source": "codex-app-server" | "grok-cli" | "static",
  "fetched_at": "2026-09-06T10:00:00.000Z",
  "models": [
    {
      "id": "gpt-6-astra",
      "display_name": "gpt-6-astra",
      "description": "…",
      "is_default": true,
      "default_effort": "medium",
      "efforts": ["low", "medium", "high", "xhigh", "max", "ultra"],
      "service_tiers": [{"id": "priority", "name": "Fast", "description": "2x speed, increased usage"}]
    }
  ]
}
```

| kind | source | 來源 | efforts | service_tiers |
|---|---|---|---|---|
| `codex` | `codex-app-server` | `codex app-server` JSON-RPC `model/list` | 每個模型自己的 `supportedReasoningEfforts`（可能含 `none/minimal/low/medium/high/xhigh/max/ultra`） | 每個模型自己的 `serviceTiers`（目前只有 `priority` = Fast） |
| `grok` | `grok-cli` | `grok models` + `~/.grok/models_cache.json` 的 per-model `reasoning_efforts`（無 cache 時退回 `["low","medium","high"]`） | 依模型（grok-4.6 含 `xhigh`；grok-4.5 為 low/medium/high） | `[]` |
| `claude` | `static` | 靜態（`opus / sonnet / haiku / fable`） | `["low","medium","high","xhigh","max"]` — `claude --help` 對 `--effort` 列的五級，**每個 alias 都一樣**（claude 沒有 per-model 清單） | `[]` |

`default_effort`：codex / grok 是那個模型自己回報的值；**claude 讀 `identity` 那個帳號的
`settings.json`**——`effortLevel`（全域）先墊底，`modelSettings.<真實 model id>.effortLevel`
（per-model 覆寫）蓋過去。真實 id（`claude-opus-5` 這種）不是啟動用的 alias，比對用子字串
（含 `opus`/`sonnet`/`haiku`/`fable` 哪個字就算命中）。兩者都沒有、檔案讀不到、或不是合法
JSON → **`"high"`**（claude 自己的內建預設；官方文件 `code.claude.com/docs/en/model-config`：
「`high`…The default on every model except Opus 4.7」，`opus/sonnet/haiku/fable` 都不是
Opus 4.7；也拿一個從未動過 effort 的乾淨帳號實測過，`/effort` 拉桿確實停在 `high`）。

- `display_name` / `description` 可能為空字串；`default_effort` claude 一律有值（見上），
  codex / grok 仍可能為 `null`（該模型沒回報預設）。
- 失敗（CLI 不存在、逾時、解析失敗、ssh 失敗）→ `502 {"error":"upstream","message":"…"}`，
  **前端應退回靜態清單**（claude：`opus / sonnet / haiku / fable` 加上那五級 effort；codex / grok 由前端自備）。
- `kind` 不合法 → 400；`host` 不存在 → `404 {"error":"not_found","what":"host"}`。

### 12.2 `bot.fast`（布林，預設 `false`）

| 欄位 | TOML | DB | `GET state` | `POST bots` | `PATCH bots` |
|---|---|---|---|---|---|
| `fast` | `fast = true` | `bots.fast INTEGER`（additive migration） | 每個 bot 物件都有 | 可省，預設 `false` | 可改；有 active Run 時列入 `needs_restart` |

啟動注入（依 kind）：

| kind | model | effort | fast |
|---|---|---|---|
| `codex` | `-m <model>` | `-c model_reasoning_effort="<effort>"` | `-c service_tier="priority"`（僅 `fast=true`） |
| `grok` | `-m <model>` | `--reasoning-effort <effort>` | 不注入 |
| `claude` | `--model <model>` | 不注入（`effort` 一律存為 `null`） | 不注入 |

**`effort` 驗證改為 kind 相依**（`POST` / `PATCH` 皆同，違反 → 400）：

- `grok`：`low | medium | high | xhigh`（啟動時若該模型不支援會被丟掉，例如 grok-4.5 不接受 xhigh）
- `codex`：`none | minimal | low | medium | high | xhigh | max | ultra`
- `claude`：任何值都被清成 `null`（不報錯）

argv 順序不變：daemon 旗標 → model → effort → fast → identity.args → bot.args。

### 12.3 `hosts[].attach_command`

`GET /api/state` 的 `hosts[]` 每項新增唯讀字串 **`attach_command`**：在使用者終端貼上即可接上該 host 的 herdr session。

| host | 指令 |
|---|---|
| `local` | `herdr --session <herdr_session>` |
| 遠端、`ssh_port = 22` | `herdr --remote <ssh> --session <herdr_session>` |
| 遠端、其他埠 | `herdr --remote ssh://<ssh>:<port> --session <herdr_session>` |

範例：`{"name":"m4p","ssh":"m4p@100.112.229.82","ssh_port":2222,"herdr_session":"agents-manager","attach_command":"herdr --remote ssh://m4p@100.112.229.82:2222 --session agents-manager", …}`

### 12.4 額度 `GET /api/quota?refresh=1&host=<name>`

額度是**按主機**分開的（SPEC §14）：本機用裸 key，遠端主機把自己的名字加在前面
（`m4p/claude`、`m4p/claude:cc1`、`m4p/codex`、`m4p/grok`），每筆另有 `host` 欄位。
每台主機（`local` + 每個 `hosts[]`）的三個基本 kind 一定都在 map 裡，沒資料就是 `null`。

```json
{
  "kinds": {
    "codex": {
      "five_hour": {"used_pct": 12.5, "resets_at": "2026-09-06T14:00:00.000Z", "low": false, "critical": false},
      "seven_day": {"used_pct": 40.0, "resets_at": "2026-09-12T08:00:00.000Z", "low": false, "critical": false},
      "plan": "pro",
      "updated_at": "2026-09-06T10:00:00.000Z",
      "source": "codex-app-server",
      "host": "local"
    },
    "claude": {
      "five_hour": {"used_pct": 97.0, "resets_at": "2026-09-06T14:00:00.000Z", "low": true, "critical": true},
      "seven_day": {"used_pct": 22.0, "resets_at": "2026-09-12T08:00:00.000Z", "low": false, "critical": false},
      "fable": {"used_pct": 39.0, "resets_at": "2026-09-12T08:00:00.000Z", "low": false, "critical": false},
      "plan": null,
      "updated_at": "2026-09-06T10:00:00.000Z",
      "source": "statusline",
      "account": null,
      "host": "local"
    },
    "claude:cc1": { "…": "同上，account = \"cc1\"" },
    "m4p/claude": { "…": "m4p 上讀到的同一組欄位，host = \"m4p\"" },
    "m4p/codex": { "…": "同上" },
    "grok": {
      "five_hour": null,
      "seven_day": {"used_pct": 14.0, "resets_at": "2026-09-12T08:28:00.000Z", "low": false, "critical": false},
      "plan": "SuperGrok",
      "updated_at": "2026-09-06T10:00:00.000Z",
      "source": "grok-usage",
      "host": "local"
    }
  }
}
```

- `kinds` 的 key：`codex`、`claude`、`grok`，以及有 identity 的 claude bot 另存一份 `claude:<identity>`
  （`account` = identity 名稱）；遠端主機的同一組 key 前面加 `<host>/`。**沒有資料的 kind 為 `null`**
  （沒裝該 CLI 就是 `null`；claude 在第一個 StatusLine 事件到達前為 `null`，grok 在第一次 `/usage`
  探測回來前為 `null`）。host 名不含 `/`，所以 key 永遠拆得回 `(host, kind[:identity])`；不屬於任何
  現存主機的 `<host>/…` key 不會出現在回應裡（主機一被移除就連同它的額度一起丟掉）。
- `host`：這筆是在哪台主機讀到的（`local` 或 `hosts[].name`）。UI 的標題列一次只顯示一台
  （看哪個 bot / 專案就是哪一台），所以遠端 bot 的 statusLine 不會蓋掉本機那列。
- `used_pct` 為 0–100 的數字；`resets_at` 為 RFC3339 或 `null`；`five_hour` / `seven_day` 任一可為 `null`。
- `fable`：Claude **Max 方案**才有的 Fable 週額度（`/usage` 的 `Current week (Fable)`），欄位型別與
  `seven_day` 完全相同（也是週窗，只是只算 Fable 那一份）。沒有這條桶子的方案／來源（codex、grok、
  非 Max 帳號）一律 `null`——**額度條在 `null` 時完全不畫這條，也不佔位**；有值時在 5h / 7d 之後多一條
  標籤 `F` 的同款長條。舊 daemon 沒有這個欄位，前端讀不到就當 `null`（相容）。
- `low` / `critical` 為 daemon 算好的門檻旗標（`daemon/src/quota.rs` 的 `LOW_REMAINING_PCT` = 30、
  `CRITICAL_REMAINING_PCT` = 5，皆用「剩餘 % = 100 − used_pct」判斷）：**門檻在 API server 端決定，
  前端只讀旗標，不得自己寫死百分比比較**。`low` → 額度條除了長條外要把剩餘數字顯示出來；
  `critical` → 該 bot 在側欄 bot 列上要有提示（用哪組額度見 §12.4 的 key 對應）。
- `?refresh=1`：立刻重讀 codex、claude（`claude -p "/usage"` pane 探測）與 grok，對象是 `local` 加上
  每一台**已連線**的遠端主機；加 `&host=<name>` 只重讀那一台（主機不存在 → 404）。claude / grok 的探測
  要開 pane（claude 最久 40 秒、grok 25 秒），多台是依序跑的。
- 背景輪詢的節奏不變（codex 5 分、claude 60 秒、grok 30 秒），每一輪把 `local` 與每一台已連線遠端
  **併發**跑一次——一次探測要數十秒，序列跑會把本機的週期拉長（SPEC §14.3）。
- 來源（每一台主機各自跑一份）：
  - **codex**：daemon 啟動後與每 5 分鐘用該主機的 `codex app-server` 的 `account/rateLimits/read`
    （遠端走 ssh）。
  - **claude**：兩路並存。
    1. **statusLine 推送**（bot 對話中）：daemon 注入的 `statusLine` 指令把 `rate_limits.five_hour / seven_day`
       （若哪天多了 `rate_limits.fable` 也會一起收；實測 2.1.263 的 payload 只有前兩個桶，所以 statusLine
       來源的 `fable` 目前都是 `null`）POST 到 `/hook/claude`（`hook_event_name = "StatusLine"`）；不建 Turn。`source` = `statusline`。
    2. **`claude -p "/usage"` pane 探測**（背景，每 60 秒）：在專屬 `am-quota` herdr session 開用完即丟的
       pane，在裡面跑**一行**指令：`claude auth status --json` 接 `claude -p "/usage"`，輸出用
       `AM_AUTH_BEGIN` / `AM_AUTH_END` / `AM_USAGE_DONE=` 三個標記包起來，用 `pane.read`
       （`source = recent_unwrapped`）等到最後那個標記出現（逾時 40 秒）。`-p` 印的是純文字，一條桶子一行：

       ```text
       Current session: 47% used · resets Sep 7 at 9:59pm (Asia/Taipei)
       Current week (all models): 15% used · resets Sep 14 at 11:59am (Asia/Taipei)
       Current week (Fable): 23% used · resets Sep 14 at 11:59am (Asia/Taipei)
       ```

       依序對應 `five_hour` / `seven_day` / `fable`（Max 方案才有第三條，標題大小寫不敏感），其餘
       model-specific 的週列（Sonnet / Opus）仍忽略；`plan` 取自同一次 `auth status` 的 `subscriptionType`。
       **不再有 TUI 對話框、first-run 信任視窗與滿意度問卷要對付**（`-p` 一律不問），也**不再用 ssh 探登入**。
       每個有獨立 `CLAUDE_CONFIG_DIR` 的 identity 各探一次（空 env / `cc0` 與預設帳號共用 `claude` key）；
       identity 清單是**該主機**的（含它 shell 的 `ccN`，SPEC §16）。跳過不探的條件：該列在 60 秒內剛被
       statusLine 更新過**且**這個身份的 `logged_in` 已經知道（登入答案是搭同一次探測回來的，所以還沒有
       答案的身份仍值得開一次 pane）。沒登入時 `/usage` 只印一段成本摘要、一條桶子都沒有 → 該身份被
       park 30 分鐘（其餘失敗 5 分鐘），下一輪再試。`source` = `claude-usage`。**遠端主機**的探測開在
       daemon 自己在那台上的 named session（遠端只有一條被轉發的 socket），cwd 與 identity env 的 `~`
       都用遠端的 `$HOME`（SPEC §14.2）。statusLine 則依 bot 所在主機寫入對應的列。
  - **grok**：CLI 沒有可查額度的介面，daemon 每 30 秒在專屬的 `am-quota` herdr session（永不 attach，
    因此版面夠寬）開一個用完即丟的 pane 跑 grok、送 `/usage`、讀回對話框文字解析（SPEC §12.6）。只回報週額度 → 放在 `seven_day`，`five_hour` 為 `null`，`plan` 取自
    `Weekly limit (SuperGrok)` 的括號，`source` = `grok-usage`。

### 12.5 WS `quota_updated`

```json
{"seq":57,"type":"quota_updated","data":{"kind":"m4p/claude","host":"m4p","quota":{ "five_hour":{…},"seven_day":{…},"fable":null,"plan":null,"updated_at":"…","source":"statusline","account":null,"host":"m4p" }}}
```

`kind` 為 `kinds` 的完整 key（含 `claude:<identity>`，遠端主機含 `<host>/` 前綴），`host` 是同一個值的
方便欄位。每次額度數值更新時推送；前端把 `data.quota` 直接寫進 `kinds[data.kind]`。

### 12.6 工具偵測 `hosts[].tools`

每個 host 連線成功時（本機為 daemon 啟動時）daemon 用該主機的登入 shell 偵測三種 CLI 是否存在、版本與登入狀態，
結果快取在 `GET /api/state` 的 `hosts[]`：

```json
{
  "name": "m4p", "…": "…",
  "tools": {
    "claude": {"installed": true,  "path": "/opt/homebrew/bin/claude", "version": "2.1.0 (Claude Code)", "logged_in": true},
    "codex":  {"installed": true,  "path": "/opt/homebrew/bin/codex",  "version": "codex-cli 0.120.0",   "logged_in": true},
    "grok":   {"installed": false, "path": null, "version": null, "logged_in": null}
  },
  "tools_checked_at": "2026-09-06T10:00:00.000Z"
}
```

- `installed`：登入 shell（`"$SHELL" -lic 'command -v <kind>'`）找得到執行檔。
- `path` / `version`：`command -v` 與 `<kind> --version` 的輸出（第一行，trim）；沒裝為 `null`。
- `logged_in`：`true | false | null`（判不了為 `null`）。判斷依據：claude `~/.claude/.credentials.json`（或 Keychain
  `Claude Code-credentials`）存在；codex `~/.codex/auth.json` 存在；grok `~/.grok/` 下有 auth 檔。
- **`hosts[].identities[]` 的 `logged_in` / `account` / `plan` 是分開一條路**：codex 與 grok 走這裡的 ssh
  探測（`codex login status` / `grok models`），**claude 不走 ssh**——非登入的 ssh session 讀不到 macOS
  Keychain，會對明明能用的帳號答 `loggedIn: false`（m4p 的 cc1 就是這樣）。claude 改在該主機的
  `am-quota` herdr pane 裡跑 `claude auth status --json`，和 §12.4 的 `/usage` 是**同一次**探測，
  `email` → `account`、`subscriptionType` → `plan`；因此 claude 身份的登入狀態在第一輪額度輪詢
  （最多 60 秒）之後才會從 `null` 變成真正的答案，變了會推 WS `host_changed`。
- host 尚未偵測（例如遠端還沒連上）時 `tools` 為 `null`、`tools_checked_at` 為 `null`。
- `POST /api/hosts/{name}/tools/refresh` → 立即重新偵測，回 `200 {"name":"m4p","tools":{…},"tools_checked_at":"…"}`
  （host 不存在 404；ssh 失敗 502）。重新偵測後亦推 WS `host_changed`（前端重新 `GET /api/state`）。

### 12.7 透過現有 agent 安裝 / 登入 `POST /api/hosts/{name}/tools/install`

```json
{ "kind": "grok", "via_bot_id": "01M1…" }
```

daemon 組一則安裝 prompt（依 kind 用官方安裝方式：claude `curl -fsSL https://claude.ai/install.sh | bash`、
codex `npm i -g @openai/codex`、grok `curl -fsSL https://x.ai/cli/install.sh | bash`（`~/.grok/README.md` 記載的官方安裝腳本）；接著要求 agent 確認
`<kind> --version`、執行登入（claude 直接執行 `claude` / codex `codex login` / grok `grok login`）並把登入 URL 原樣印出），
走既有的 §5 prompt 路徑送給 `via_bot_id`（`client_request_id` 由 daemon 產生）。

回應 `200`：

```json
{ "turn_id": "01M1…", "message_id": "01M1…", "delivery": "ok" }
```

- `via_bot_id` 不存在 → `404 {"error":"not_found","what":"bot"}`；bot 不屬於該 host → `400`；
  bot 沒有 running 的 Run / blocked / in-flight → 與 §5 相同的 `409`。
- `kind` 不合法 → 400。
- 登入是互動式的：agent 執行 `login` 後 pane 會變 `blocked`，使用者在 UI 的終端快照處理即可。
- 安裝完成後前端可呼叫 `POST /api/hosts/{name}/tools/refresh` 更新 `tools`。

### 12.8 bot 人設 `bot.persona`

每個 bot 新增可選欄位 **`persona`**（`string | null`，預設 `null`）：一段附加到 agent system prompt 尾端的文字
（使用者的「附加在 agent 的 md 最後」）。**不會**動到專案目錄裡共用的 `CLAUDE.md` / `AGENTS.md`。

| 欄位 | TOML | DB | `GET state` | `POST bots` | `PATCH bots` |
|---|---|---|---|---|---|
| `persona` | `persona = """多行字串"""` | `bots.persona TEXT`（additive migration） | 每個 bot 物件都有（`string \| null`） | 可省 | 可改，`null` / `""` 清除；有 active Run 時列入 `needs_restart` |

啟動注入（有值才注入；位置在 daemon 旗標之後、model / effort 之前；遠端主機同樣走 argv，不需額外檔案）：

| kind | 注入 |
|---|---|
| `claude` | `--append-system-prompt "<persona>"` |
| `grok` | `--rules "<persona>"`（`grok --help`：Extra rules to append to the system prompt） |
| `codex` | `-c developer_instructions=<TOML 字串>`（daemon 以 TOML basic string 逃逸換行與引號；實測結果見 PROGRESS v4.0） |

argv 順序：daemon 旗標 → persona → model → effort → fast → identity.args → bot.args。

### 12.9 GitHub 專案偵測 `projects[].github` 與 issues

daemon 在專案載入 / 對帳 / `POST /projects` 時偵測 git origin（本機 `git -C <path> remote get-url origin`，遠端經 ssh 同指令），
解析 `git@github.com:owner/repo.git`、`https://github.com/owner/repo(.git)`、`ssh://git@github.com/owner/repo`，放進
`GET /api/state` 的每個 project：

```json
{ "id": "01M1…", "path": "…", "label": "powertech-hub", "host": "local",
  "github": {"owner": "Eden-Sun", "repo": "powertech-hub", "url": "https://github.com/Eden-Sun/powertech-hub"},
  "bots": [ … ] }
```

- 非 GitHub、沒有 remote、或 git 指令失敗 → `github: null`。結果快取到下次對帳。
- `POST /api/projects/{id}/github/refresh` → 立即重測，回 `200 {"project_id":"…","github":{…}|null}`；並推 `project_changed`。

#### `GET /api/projects/{id}/issues?state=open|closed|all&limit=30&q=<關鍵字>&refresh=1`

用該主機的 `gh issue list --repo owner/repo --state <s> --limit <n> [--search "<q>"] --json …` 取得。
`state` 預設 `open`；`limit` 1–100（預設 30）；`q` 可省。daemon 快取 **2 分鐘**（key = project + state + q + limit），`refresh=1` 跳過。

```json
{
  "project_id": "01M1…", "repo": "Eden-Sun/powertech-hub", "source": "gh",
  "fetched_at": "2026-09-06T10:00:00.000Z",
  "issues": [
    {"number": 42, "title": "…", "state": "OPEN", "labels": ["bug"], "url": "https://github.com/Eden-Sun/powertech-hub/issues/42",
     "updated_at": "2026-09-05T12:00:00Z", "author": "Eden-Sun", "body_excerpt": "前 300 字，換行壓成空白"}
  ]
}
```

- `project.github` 為 `null` → `400 {"error":"bad_request","message":"project has no GitHub origin"}`。
- `gh` 不存在 / 未登入 / 執行失敗 → `502 {"error":"upstream","message":"gh 未安裝或未登入…"}`。
  遠端主機請走 `POST /api/hosts/{name}/gh/login`（見上方），不要叫使用者自己 ssh。
- project 不存在 → 404。

#### `GET /api/projects/{id}/issues/{number}`

單一 issue 的完整內容（`gh issue view <n> --repo … --json …`，不快取）：

```json
{"project_id":"…","repo":"owner/repo","issue":{"number":42,"title":"…","state":"OPEN","labels":["bug"],"url":"…",
 "updated_at":"…","author":"…","body":"完整 markdown 內文"}}
```

錯誤同上（找不到 issue 也是 502，message 含 gh 的輸出）。

#### submodule 的 issue（2026-09-07 新增）

專案若有 git submodule，submodule 自己的 GitHub issue 也能看、也能組隊。

`GET /api/projects/{id}/submodules?refresh=1` — `.gitmodules` 列出的每一個，帶各自的 origin（快取 2 分鐘）：

```json
{"project_id":"…","submodules":[{"path":"vendor/foo","github":{"owner":"acme","repo":"foo","url":"https://github.com/acme/foo"}},
                                {"path":"tools/bar","github":null}]}
```

上面兩個 issue 端點都多一個查詢參數 `repo=<submodule path>`（相對於專案根目錄；省略或空字串 = 專案本身），
回應多 `repo_path` 回顯。`repo` 不在 submodule 清單裡 → `400`；該 submodule 沒有 GitHub origin → `400`。

`POST /api/projects/{id}/teams` 的 body 也接受 `repo`：team 的 worktree、分支、合併、PR 與關 issue 全部在**那個 submodule 的 repo** 裡進行；
team 物件多 `repo` 欄位（`""` = 專案本身）。詳見 SPEC-team §2.4。

#### `POST /api/teams/{id}/issues` 的 409（2026-09-07 修正）

追加 issue 到 team 佇列（`{"issue_numbers":[57,58]}`，或單數 `{"issue_number":57}`），成功回 `200 {"issues":[…]}`。
409 的 `reason` 有三種：

| `reason` | 何時 | extra |
|---|---|---|
| `team is finished` | `aborted` / `failed` 的 team（`done` 且未 cleanup 則放行並 reopen，SPEC-team §2.5） | `phase` |
| `team is cleaned up` | `done` 但已 cleanup（沒有活著的 PM 可以接手） | `phase` |
| `issue already queued` | 同號 issue **還在佇列上**，也就是該 `team_issues` 列的 `state` 是 `queued` 或 `working`；同一個請求裡自己重複（`[57, 57]`）也算 | `issue_number` |

**`issue already queued` 只看還在佇列上的列**：`done` / `failed` / `skipped` 是做完那一趟的紀錄，
同一個 issue **可以再排一次**（失敗後重試、或想再做一趟），舊列留著、新列取下一個 `seq`，
第 n 趟的整合分支是 `team/i<issue>-<tid6>-r<n>`。詳見 SPEC-team §2.3「再排同一個 issue」。
> 修正前是拿**全部**列（含 `done` / `failed` / `skipped`）比對 issue 號，所以一個 issue 在同一隊做過一次
> 就永遠不能再排，UI 只會看到「追加 issue 失敗：issue already queued」。

## 子 agent（bot 自己開的 pane，2026-09-07 新增）

daemon 起的每個 agent 都帶一段預設人設（`lifecycle::child_agent_rules`，接在使用者的 `bot.persona` 前面）：
自己的 agent 名稱、子 agent 的命名前綴、`herdr pane split --pane "$HERDR_PANE_ID"`、
不要 `git stash` / `--autostash`、子 agent 會被掛在自己底下追蹤。三種 kind 都用同一份文字
（claude `--append-system-prompt`、grok `--rules`、codex `developer_instructions`）。

**claude 另外拿到 herdr skill**（SPEC §6.5c）：啟動前 daemon 把 `herdr --skill` 寫到
`$CLAUDE_CONFIG_DIR/skills/herdr/SKILL.md`（預設 `~/.claude/skills`），frontmatter 的 `description`
換成 AG Man 的版本（原文是「只有使用者明確提到 Herdr 才用」，這裡改成「需要開子任務 / 平行工作就用」），
body 最前面插上同一份 AG Man 規則。內容相同就不寫。

對帳（`reconcile`）時，herdr 裡沒有任何 bot 認領的 agent 會被建成某個 bot 的**子 bot**：
`managed_by = "child"`、`parent_bot_id = <父 bot id>`、kind 取 herdr 偵測到的（偵測不到就沿用父的）、
identity 沿用父的、不注入 hook（回覆走終端擷取）。同時建一筆 `adopted = 1` 的 run，之後跟一般 bot 一樣有燈號、對話、終端。

認父的線索有兩條，**血緣優先**（SPEC §6.5a）：

1. **血緣**：一個 bot 一個 tab，所以子 pane 一定 split 在父的 tab 裡。這個 agent 的 `tab_id` 等於某個 bot
   活動 run 的 `tab_id` → 就是那個 bot 的子 agent。人設只是請求（agent 會忘、codex / grok 可能沒讀），
   tab 是事實，因此不管子 agent 叫什麼名字都追得到。同一 tab 內有父也有已認領的子時取名字前綴最長的，
   平手取非 `child` 的那個，孫代因此掛在子代下面。
2. **名字前綴**：`<某 bot 的 agent_name>-<字尾>`，最長匹配。跨 tab 與 team workspace 只有這條。

子 bot 的 `name`：有前綴就取字尾，否則用 herdr 的 agent 名（去掉空白與 `@,:;`、截到 32 字）。

**herdr PATH shim**（SPEC §6.5b）：daemon 在 `<bot 目錄>/bin/herdr` 放一支 `sh` 包裝腳本並放到 pane 的 `PATH` 最前面，
把命名從「請求」變成「機制」——`herdr agent start <名稱>` 會自動補上 `$AM_AGENT_NAME-` 前綴，
`herdr pane split` / `tab create` 會自動用 `--env` 把父的帳號（`CLAUDE_CONFIG_DIR` / `CODEX_HOME`）與
hook 環境（`AM_BOT_ID` / `AM_HOOK_TOKEN` / `AM_PORT` / `AM_RUN_ID` / `AM_AGENT_NAME`）帶進子 pane，
其餘子指令原樣轉發。pane env 因此多 `AM_AGENT_NAME`。子 agent 指定自己的 pane 用 herdr 注入的 `$HERDR_PANE_ID` 或 `--current`。

- `GET /api/state` 的 bot 物件多 `parent_bot_id`（頂層為 `null`），`managed_by` 多一個值 `child`。
- 子 bot 不進 config.toml；pane 消失時 daemon 把它 `deleted_at`（對話保留）。`DELETE /api/bots/{id}` 對子 bot 直接停 pane 並軟刪。
- UI：側欄把子 bot 縮排掛在父 bot 底下。
- 子 bot 的**對話**來自終端擷取（SPEC §4.3）：pane 不是 daemon 開的、沒有注入 hook，所以 pane 的
  `working → idle` 就是回合邊界——daemon 從 `recent_unwrapped` 快照擷取這一段（沿用既有的雜訊過濾與回音剝除），
  寫成一筆 `origin = external` / `status = completed_fallback` 的 Turn：prompt 回音存成 user 訊息、抽出的回覆
  存成 `source = terminal_fallback`、`incomplete = 1` 的 assistant 訊息。認領當下 agent 還在 `working` 的話
  改開一筆 in-flight Turn，回覆就跟一般 bot 一樣即時串流；已經 idle 則只在對話**還是空的**時候補記螢幕上那一輪。
- 子 bot 的 `model` / `effort` 從 pane 的 `pane.process_info` argv 反推（claude `--model` / `--effort`、
  codex `-m` / `-c model_reasoning_effort=…`、grok `-m` / `--reasoning-effort`，grok 另可退回終端標題
  `Grok 4.6 (xhigh)`），認領時寫進 `bots.model` / `bots.effort`，於是 `GET /api/state` 的 `model` / `effort`
  與側欄徽章直接就對。解析不到留 `null`（UI 顯示「預設」）；**只補空值**——TUI 裡的 `/model` 改不到 argv，
  已經記錄的值不會被下一次對帳蓋回去。

## 圖片附件（2026-09-06 新增）

CLI agent 只吃文字（`agent.prompt`），所以「拖一張圖進對話」是**先把檔案放到 bot 所在主機**，
再把路徑寫進 agent 讀到的那段文字。

### `POST /api/bots/{id}/attachments?name=<檔名>`

body 直接是圖片位元組（**不是** multipart），`Content-Type` 就是圖片的 MIME：

```
POST /api/bots/01.../attachments?name=screenshot.png
Content-Type: image/png
X-AM-Token: <token>
<raw bytes>
```

成功 `200`：
```json
{"id":"01M1…","name":"screenshot.png","mime":"image/png","size":10158,
 "path":"/Users/me/proj/.agents-manager/attachments/01M1…-screenshot.png"}
```

- 檔案落在 **`<project.path>/.agents-manager/attachments/`**（agent 的 cwd 之內，沙箱化的 CLI
  才讀得到）；該目錄會自動寫一個內容為 `*` 的 `.gitignore`，repo 不會看到這些檔案。
- 專案在遠端 host 時，位元組經 `ssh`（`hosts.rs::ssh_put`）寫到遠端同一路徑，daemon 另存一份
  本機副本供 UI 取縮圖。
- 只收圖片（`Content-Type` 必須是 `image/*`），單檔上限 12 MB；其餘 `400 bad_request`。

### `GET /api/attachments/{id}`

回傳原始位元組（`Content-Type` 為原 MIME）。一樣要 `X-AM-Token`，所以 UI 是用 fetch 取回再轉
object URL，不能直接塞進 `<img src>`。

### prompt / 群組聊天帶附件

`POST /api/bots/{id}/prompt` 與 `POST /api/projects/{id}/chat` 都多接一個可選欄位：

```json
{ "text": "這張圖哪裡怪？", "client_request_id": "…", "attachments": ["01M1…", "01M1…"] }
```

- attachment id 以 **project** 為範圍：同專案的 bot 共用（群組聊天一次上傳、每個收件 bot 都拿到
  同一個路徑）；跨專案的 id 會 `400 unknown attachment`。
- agent 實際收到的是「文字 + 附加圖片（請讀取這些檔案來查看）：<絕對路徑>」；時間軸存的仍是
  使用者原本打的字。
- 這些圖片會記在該則 user message 的 `attachments_json`（`GET /messages` 一併回傳），格式是
  上面 upload 回應的物件陣列，UI 靠它重畫縮圖。

### `run.agent_title`（2026-09-06 新增）

`GET /api/state` 的 `bots[].run` 與 `bot_status` 事件多一個欄位：

```json
{"id":"01M1…","state":"running","agent_status":"working","agent_title":"V40-OK", …}
```

- 來源是 herdr `agent.list` 的 `terminal_title_stripped`——**agent 自己替當前工作取的名字**
  （Claude Code 會寫成任務摘要，codex / grok 通常是目錄名或狀態）。
- herdr 沒有「標題變了」的事件，所以 daemon 每 4 秒對每個已連線的 host 做一次 `agent.list`
  （`events::spawn_title_poller`），只有真的變了才寫 DB 並推 `bot_status`。
- 存進 `runs.agent_title`（新欄位）。寫入前會過濾：前後的 `-` 去掉（grok 把狀態寫進標題，
  像 `- Thinking - <task> - grok`），標題若只是 CLI 自己的名字（`Claude Code`、`codex`…）
  就當作沒有，維持 `null`。
- run 結束後不會清除，但 UI 只在 run 還活著時讀它。

### `run.status_line`（2026-09-06 新增）

`GET /api/state` 的 `bots[].run` 與 `bot_status` 事件再多一個欄位：bot 自己那條狀態列的原文。

```json
{"id":"01M1…","status_line":"hunta | agents-manager | OP5 42% | 5h:59%(rst 3h 25m) | 7d:73%(rst 5d 4h) | F5:61%"}
```

- 來源是**使用者自己的** claude `statusLine` 命令。daemon 的 `agents-managerd statusline`
  本來就會代跑它並把 stdout 原樣送回 pane（v4.0）；現在同一份輸出（ANSI 已 strip）也放進
  POST 給 `/hook/claude` 的 payload（`status_line`），存進 `runs.status_line`。
- 只有 claude 有：codex / grok 沒有 statusLine 機制，欄位維持 `null`；使用者沒設定
  `statusLine.command` 時也是 `null`。
- claude 刷新得很勤，所以 daemon 只在文字**變了**才寫 DB 並推 `bot_status`。
- 順序上，使用者的命令跑在 POST 之前（要拿它的輸出），整體仍在 statusline 的 1.9 秒預算內。

### `bot.herdr_session` / `run.herdr_session`（2026-09-06 新增）

- 一般 Bot 的 `herdr_session` 為 `null`，表示沿用 Project host 的設定 session。
- 從使用者本機 `default` session 自動採用的 Bot 會帶 `herdr_session: "default"`，其 active
  Run 也會帶相同值；這讓 prompt、keys、terminal 與狀態事件不會送到 manager 的 named session。
- default session 的採用條件是支援的 agent kind 且 `foreground_cwd` / `cwd` 與既有 local
  Project 路徑完全相同；普通 pane 不會出現在 state 裡。

### `run.status_json`（2026-09-06 新增，接續 `run.status_line`）

`status_line` 是使用者腳本壓縮過的一行；`status_json` 是**壓縮前**的原始資料，給網頁用
（`statusline_cmd` 除了 `transcript_path` 之外整份轉發）：

```json
{"account_email":"…@gmail.com","model":{"display_name":"Opus 5 (1M context)","id":"claude-opus-5"},
 "context_window":{"used_percentage":44,"total_input_tokens":442000,"context_window_size":1000000},
 "rate_limits":{"five_hour":{"used_percentage":55,"resets_at":1788671400}, "seven_day":{…}},
 "cost":{"total_cost_usd":50.89}, "effort":{"level":"high"}, "thinking":{"enabled":true},
 "session_name":"…", "version":"2.1.261", "workspace":{"current_dir":"…"}}
```

- `account_email` 是 daemon 補上的（payload 本身沒有）：照使用者腳本的做法讀
  `.claude.json` 的 `oauthAccount.emailAddress`，identity 決定是哪個設定目錄，因此
  cc0 / cc1 各自對得上自己的帳號。
- 一樣只在內容變動時寫入並推 `bot_status`。
