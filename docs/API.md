# agents-managerd HTTP / WebSocket API

daemon 預設 `http://127.0.0.1:7788`（`config.toml` 的 `server.listen`）。行為與理由在 `SPEC.md`，這份只寫契約；前端以此為準。
時間欄位一律 RFC3339 UTC（毫秒）；id 為 ULID 字串。

## 0. 認證

1. `GET /api/session` 不需 token，但**連線的 TCP 對端**必須是 loopback（不看 `Host`：那是呼叫端自己填的），`Origin`（若有）的主機必須是 `127.0.0.1` / `localhost` / `[::1]`（不限 port）。回 `{"token":"<32 hex>","port":7788}`。
   開發版（`allow_lan`，見 SPEC §7.1）這兩項都直接放行：同網段誰都拿得到 token——這是使用者裁示保留的風險（`e7392dd`）。
2. 其餘 `/api/*` 需 header `X-AM-Token`；缺或錯 → `401 {"error":"missing or bad X-AM-Token"}`。
3. WebSocket：`/ws?token=<token>[&since=<seq>]`。
4. `/hook/*`、`/relay/announce`、`/relay/pane` 用 per-bot 的 `X-AM-Bot-Token`。

Vite proxy 要把 `/api`、`/ws`（含 upgrade）、`/hook` 轉到 daemon。daemon 不檢查 `Host`；proxy 從本機連過來，對端就是 loopback。

## 1. 錯誤格式

| 狀態碼 | body | 意義 |
|---|---|---|
| 400 | `{"error":"bad_request","message":"..."}` | 參數錯誤 |
| 401 | `{"error":"..."}` | token 錯 |
| 403 | `{"error":"..."}` | 對端（只有 `/api/session`）／`Origin` 非本機；另有各端點自己的 403（例如 `read_only_pane`） |
| 404 | `{"error":"not_found","what":"bot"\|"project"\|"run"\|"pane"\|"turn"}` | 找不到 |
| 409 | `{"error":"conflict","reason":"<人類可讀>", ...extra}` | 狀態機衝突；extra 視情況含 `run_id` / `turn_id` / `bot_id` / `name` / `path` / `state` |
| 502 | `{"error":"upstream","message":"..."}` | herdr / DB 出錯 |
| 503 | `{"error":"start_state_uncommitted"\|"stop_state_uncommitted"\|"restart_state_uncommitted","run_id","retryable":true,"message","detail"}` | 外面的副作用已經做了（agent 起來了／停了），run 的狀態卻寫不進 DB；daemon 已排重試，run 會照 herdr 的證據收斂（SPEC §6.2、§6.4）。不是「沒做」也不是「做好了」：看 bot 狀態，或稍後重送。另一種 503 是「讀不到狀態所以一步都沒做」（`sent:false`，帶 `Retry-After`，例如 prompt 的 `maintenance_state_unavailable`），兩者 body 分得開 |

所有寫 config.toml 的 API（建/改專案、建/改 bot、排序、還原、建/刪身分）套用、驗證、DB-backed 大量軟刪
閘門都在**落盤之前**做完（issue #73，統一 commit boundary：全部走 `projection::update_and_project`），
閘門擋下來時回
`409 {"reason":"projection_refused", message, config_written:false, bots, projects}`：config.toml 與 SQLite 都沒被動過，
`bots`／`projects` 是這次若寫入會被軟刪的名字，用來判斷是不是 config 已經跟 DB 對不上（該去 config.toml 補回那幾列，或改小這次
的範圍），**直接重送同一個請求**即可，不必先回頭收拾。

設定本身不合法（bot kind、identity 綁定與 kind 不符、id／名字格式）回
`400 {"error":"config_invalid", message, config_written:false}`——同樣不是 502、同樣落盤之前就被擋下來（SPEC §3.1）。
`message` 保留原因並附「（config.toml 未變更）」。

沒有伴隨 mutation 的重投（daemon 啟動、supervisor 背景巡邏定期把既有 config 套進 DB）不經過使用者請求，
仍是舊行為：擋下來時 `config_written:true`（這次的變更——如果有的話——已經在 config.toml 裡、只是還沒套用），
帶 `allow_env` 提示（`AM_ALLOW_BULK_DELETE=1`，只放行啟動那一次投影）。

## 2. `GET /api/state`

一次取回整棵樹。前端啟動、收到 `resync`、`project_changed` / `bot_changed` 時重拉。

```json
{
  "daemon_seq": 6,
  "connected": true,
  "default_connected": true,
  "herdr_session": "agents-manager",
  "projects": [
    {
      "id": "01M1S2SQPS9TA1DYRKNYCF2SJK",
      "path": "/Users/me/project/agents-manager",
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
          "primary": false,
          "run": null,
          "asleep": null,
          "lamp": "offline",
          "unread": 0, "read_mark": {"at": "2026-09-15T08:00:00.000Z", "id": "01M…"},
          "queued_turn": null
        }
      ]
    }
  ]
}
```

- `queued_turn`：排在下一個要送的 Turn（`status = "queued"`，§5），沒有就 `null`；前端據此把輸入框畫成「已排隊」。**生產者兩個**：對方回合中時 AGM 的派工（2026-09-16），與 bot 沒在跑時帶 `start_if_stopped` 的送出（issue #122，turn 帶 `awaits_start:1`）。使用者對**回合中**的 bot 送 `/prompt` 仍是 409，見 SPEC §4.4a。
- `unread` 固定 `0`（未讀由前端算）。
- 其他欄位（hosts、identities、bot 的 model/effort/fast/persona/instruction_files/identity/managed_by/parent_bot_id、run 的 runtime_* 等）見各節。

### `run` 物件（`null` = 沒有 active Run）

```json
{
  "id": "01M1...", "bot_id": "01M1...",
  "state": "starting" | "running" | "stopping" | "stopped" | "exited",
  "agent_status": "idle" | "working" | "blocked" | "unknown",
  "workspace_id": "w1", "pane_id": "w1:p2", "adopted": 0,
  "native_session_id": null, "transcript_path": null,
  "resume_session_id": null, "resume_outcome": null,
  "last_read_revision": null, "last_read_tail_hash": null,
  "started_at": "2026-09-05T15:30:00.000Z", "ended_at": null,
  "agent_status_since": "2026-09-05T15:31:20.000Z"
}
```

- `resume_session_id`／`resume_outcome`：`?resume=native` 起的 run 要接回哪個 session，與接回的結論（issue #92）。
  `resume_session_id` 在 CLI 回報 session 之後清成 `null`；`resume_outcome` 是 `"verified"`（回報的就是那段）、
  `"mismatch"`（CLI 開了新對話，對話裡有 system 說明）、`"unverified"`（claude 等滿 120 秒都沒回報，刻意放行並插說明；
  之後才到的回報會把它改成前兩者之一），沒要求接回或還在等是 `null`。還在等的 claude run 不收 prompt（SPEC §6.5.2 第 4 點）。
- `agent_status_since`：`agent_status` 最後一次**真的改變**的時間（同值重寫不算），daemon 觀察到的，
  不是任何一個前端看到的時間。`null` = 這個 run 還沒真的變過狀態，或升級前的舊列。前端算「跑了
  多久」（SPEC §2.2）以這欄為準，只有它是 `null` 時才退回這回合最早那筆 in_flight turn 的
  `created_at`，兩個都沒有才用網頁自己的觀察時間墊底。

### `asleep`（2026-09-17 新增，SPEC §6.11）

`null` = 一般狀態。有值 = 這顆是 **AGM 因為閒置超過 90 分鐘收起來省 RAM 的**，不是壞掉、也不是
使用者關的：

```json
{"since": "2026-09-17T04:10:00.000Z", "idle_minutes": 93}
```

它的 `run` 是 `null`、`lamp` 是 `offline`，但下次要用時會自動用 `--resume` 接回原本那段對話——
送訊息（`POST /api/bots/{id}/prompt`）或按啟動（`POST /api/bots/{id}/start`）都會叫醒它，
前端不必先自己啟動。`GET /api/supervisor/state` 的 bot 物件有同一個欄位。

### `lamp`（SPEC §2.2）

| 值 | 顏色 | 條件 |
|---|---|---|
| `disconnected` | 灰 | daemon ↔ herdr（或該 host）斷線 |
| `offline` | 離線灰 | 無 active Run |
| `starting` / `stopping` | 黃閃 / 黃 | run.state |
| `idle` / `working` / `blocked` / `unknown` | 綠 / 藍動畫 / 紅 / 灰黃 | running + agent_status |

## 3. 設定變更（寫回 config.toml）

| 方法 | 路徑 | body | 回應 |
|---|---|---|---|
| POST | `/api/projects` | `{"path":"/abs/or/~/path","label"?:"foo","host"?:"m4p"}`（`label` 預設目錄名） | `200 {"project_id"}`；路徑不存在 400；重複 409 |
| DELETE | `/api/projects/{id}` | — | `200 {}`；仍有 active Run → 409 |
| PATCH | `/api/projects/{id}` | `{"label":"新名字"}` | `200 {"project_id","needs_restart":false}`；trim 後為空 400。不擋 active run（agent 身分取自 bot id，label 只影響**下次啟動**的 `agent_name` slug） |
| POST | `/api/projects/{id}/bots` | `{"name","kind":"claude"\|"codex"\|"grok","args":[],"autostart":false,"inject_hooks":true,"name_auto":false, model?, effort?, fast?, identity?, persona?, instruction_files?, auto_approve?}` | `200 {"bot_id","name"}`；名稱重複 409，`instruction_files` 不合法 400（§12.8b），但 `name_auto:true` 時自動往後找 `<base>-<n>`（回應 `name` 是實際用的） |
| PATCH | `/api/bots/{id}` | 見 §10.2 | `200 {"needs_restart":bool}` |
| POST | `/api/bots/{id}/fork` | `{"name"?}` | 見 §10.3b |
| POST | `/api/bots/{id}/promote` | `{"name"?,"model"?,"effort"?}` | 子 agent 升級成頂層 bot，見 §10.3c |
| DELETE | `/api/bots/{id}` | — | 見 §10.4 |
| POST | `/api/order` | `{"projects"?:["pid",…],"bots"?:{"pid":["bot_id",…]}}` | `200 {"ok":true}`；兩個都沒有 400 |

成功後推 `project_changed` / `bot_changed`。

**`POST /api/order`**：側欄排序 = config.toml 的陣列順序，`GET /api/state` 的順序就是權威（前端不另存）。只送要改的那一半；沒列到的維持原相對順序接在後面；
config.toml 裡沒有的 id（child、已刪）忽略。成功推 `project_changed`。

**`bot.primary`**、**`bot.auto_approve`** 等欄位語意見 §10。

## 4. Run 控制

| 方法 | 路徑 | body | 回應 |
|---|---|---|---|
| POST | `/api/bots/{id}/start` | — | `200 {"run_id"}`；已有 active Run → `409 {"reason":"active run already exists","run_id"}`；`herdr_session = "default"` 的 bot（SPEC §6.5.1）→ `409 {"reason":"default_session"}`；herdr 失敗 502；agent 起來了但 `running` 寫不進去 → `503 start_state_uncommitted`（SPEC §6.2 第 7 步） |
| POST | `/api/bots/{id}/start?resume=native` | — | 接回 DB 記的原生對話（SPEC §6.5.2）：`200 {"run_id","resumed","session_id","resume_outcome"}`（`resumed` 照 `runs.resume_outcome` 說：`verified`→`true`＋回報的 session；`mismatch`→`false`、`session_id:null`；還沒回報或 `unverified`→`true`＋帶出去的 session；這次沒帶 `--resume`→`false`。不看 SessionStart 一到就清掉的 `resume_session_id`，issue #107）；接不回**不啟動**、不開新對話 → `409 {"reason":"cannot_resume","resumed":false,"resume_reason":"no_session_id"|"transcript_missing"|"unsupported_kind","bot_id"}`，由呼叫端決定要不要改成不帶 `resume` 重送。`resume` 只認 `native`，其他值 400 |
| POST | `/api/bots/{id}/stop` | — | `200 {}`；沒有 Run（或讀完之後已被 pane-exit 收掉）→ `204`。default session 的 bot 只送 ctrl+c、不關 pane。`stopping` 寫不進去 → 502，什麼都沒動；in-flight Turn 寫不進 failed → `503 {"error":"turn_state_unwritable","run_id","turn_id","retryable":true}`＋`Retry-After`，什麼都沒動（run 放回 running）；agent 沒停下來或問不到 herdr → `502`，message 以 `stop_not_confirmed` 開頭，run 不記成停止（還活著就放回 running）；停了但 `stopped` 寫不進去 → `503 stop_state_uncommitted`（SPEC §6.4） |
| POST | `/api/bots/{id}/interrupt` | `{"turn_id"?}` | `200 {}`（送 `esc`，in-flight Turn 標 failed）；herdr 拒收 `esc` → 502，Turn 維持 in-flight；`esc` 送出但 herdr 沒回：本機 claude／codex 的 log 裡已經有這次的中斷紀錄 → 照 `200 {}`（#223：claude 2.1.276+ 按 Esc 不送任何 hook），還看不到 → `409 {"reason":"interrupt_unconfirmed","turn_id","esc_sent":"unknown","retryable":true}`，Turn 維持 in-flight、等 log 裡的中斷紀錄（或回聲）；`esc` 生效但 Turn 狀態寫不進去 → `503 {"error":"interrupt_state_uncommitted","run_id","turn_id","esc_sent":true,"retryable":true}`（daemon 自己補，重試不再按 `esc`）；帶 `turn_id` 而那一筆已不在飛 → `409 {"reason":"turn_not_in_flight","turn_id","in_flight_turn_id","esc_sent":false}`，不按 `esc`。見 SPEC §6.4 |
| POST | `/api/bots/{id}/abort` | — | `200 {"aborted":["<turn_id>",…],"keys_sent":true,"key_error":null}`，見 §4.2 |
| POST | `/api/bots/{id}/keys` | `{"keys":["y"],"expect_run_id"?}` | `200 {}`；`expect_run_id` 不符 409 |
| POST | `/api/bots/{id}/text` | `{"text":"多行\n也可以","enter"?:true,"expect_run_id"?}` | `200 {}`；沒有 pane 404；`expect_run_id` 不符 409 |
| POST | `/api/turns/{id}/abandon` | — | `200 {}`；只有 `in_flight`（含 `delivery=unknown`）可放棄，其餘在 per-bot lock 內 CAS 判定為 `409 {"reason":"turn is neither in-flight nor of unknown delivery","turn_id"}`（不新增 system message） |
| POST | `/api/turns/{id}/withdraw` | — | `200 {}`；issue #122：撤回一則 bot 沒在跑時送、還在等它起來的 `queued`（`awaits_start:1`）——標 `failed`＋一則 system 說明，不送。其他狀態（已被佇列領走、AGM 的派工…）一律 `409 {"reason":"turn is not waiting for its bot to start","turn_id","status"}`、原樣不動（已經打進去的不能假裝沒送） |
| POST | `/api/bots/{id}/login` | — | `200 {"run_id","kind","command":"/login"}`，見 §4.1 |

- `keys` 的鍵名由 herdr 驗證，常用 `enter`、`esc`、`y`、`n`、`up`、`down`、`ctrl+c`。
- **文字用 `/text` 不用 `/keys`**：`\n` 不是鍵名。`/text` 走 `pane.send_text`（herdr 眼中的貼上，換行保留），`enter`（預設 true）後另送 `enter` 鍵才是送出。
  不看 `agent_status`（用途就是回合中途補一句）。

### 4.1 登入 / 切換帳號
把登入 slash 指令打進**正在跑的** bot 的 TUI；之後 agent 停在登入畫面，使用者完成前不能工作。登入結果由 `POST /api/hosts/{name}/tools/refresh` 重新偵測。

| kind | 指令 |
|---|---|
| `claude` / `grok` | `/login` |
| `codex` | 無（TUI 只有 `/logout`，要在外面跑 `codex login`；見主機層登入 §身份） |

| 狀況 | 回應 |
|---|---|
| 沒有這個 bot | `404 {"error":"not_found","what":"bot"}` |
| kind 沒有登入指令 | `400 {"error":"login_unsupported","kind":"codex","message"}` |
| 沒在跑 / 不是 running | `409 {"reason":"not_running"}` |
| agent `working` / `blocked` | `409 {"reason":"agent_busy"}` |
| 有回合進行中 | `409 {"reason":"turn_in_flight"}` |
| run 沒有 pane | `409 {"reason":"no_pane"}` |
| herdr 拒絕 | 502 |

### 4.2 強制中止 `POST /api/bots/{id}/abort`
`interrupt` 在 `esc` 送不出去時整個失敗、輸入框鎖死；`abort` **先保證解鎖**：
- `esc` 盡力送一次，結果寫在 `keys_sent` / `key_error`，不影響其餘步驟。
- in-flight 回合標 `failed`，對話留一則「回合已由使用者強制中止」；同 bot 的 `delivery = "unknown"` 回合一併收掉。
- 沒有 active run 不是錯誤。bot 不存在 404；沒有卡住的回合 → `200 {"aborted":[]}`（冪等）。agent 可能還在跑，要停就 `stop`。

## 5. 送訊息

`POST /api/bots/{id}/prompt`

```json
{ "text": "Reply with exactly PONG", "client_request_id": "<前端產生的唯一字串>", "relay_from": "<bot id> | \"daemon\"", "attachments"?: ["<attachment id>"], "send_now"?: true, "start_if_stopped"?: true }
```

- `relay_from` 省略 = **使用者自己打的**。其他來源一定要帶：另一顆 bot 帶它的 `bot_id`，launchd 腳本與 daemon 的自動通知帶 `"daemon"`。不是存在中的 bot 也不是 `daemon` → 400。
  寫入 `messages.relay_from`，UI 據此畫「誰 → 誰」。
- `client_request_id` 可省（daemon 補），建議自帶：同 id 重送回同一個 `turn_id`（即使已有新 Turn 在飛）。

成功 `200 { "turn_id", "message_id", "delivery": "ok" | "unverified" | "unknown" | "failed" }`：

- `ok`：已送達，等 hook（或終端備援）。
- `unknown`：RPC 逾時；該 Turn 仍 in_flight 時，**abandon / interrupt / stop 之前不能再送**（409）。Stop hook 完成時會收成 `ok`。UI 顯示「送出狀態不明」並提供放棄按鈕。
- `failed`：agent 當下 blocked 或附件綁定失敗；Turn 直接 failed，並推 `message_added`（system）與 `turn_updated`。Turn 收不成 failed 時不回這個，回下面的 503（`sent:false`，#158）。

判斷送達看回應有沒有 `message_id`：409 的 body 也可能帶 `turn_id`。

**送出之後結果寫不回 DB**（#149）→ `503 {"error":"delivery_state_uncommitted","run_id","turn_id","message_id","delivery","sent","retryable":true,"message","detail"}`：
prompt 已經送出（或 herdr 已經拒收），或確定一個字都沒送（附件綁不上、撤不回來、插隊送出的鍵沒生效，#158），只是那一筆回合的結果還沒寫成——
不要換新的 `client_request_id` 重送。
`delivery` 是看到的結果（`ok`／`unverified`／`unknown`／`failed`），`sent` 講字有沒有進去（`failed`＝沒進去＝`false`，`unknown`＝`null`）。
daemon 自己補（定時重試、下一則 prompt、回合 hook）；同一個 `client_request_id` 重問拿到寫好的結果，還沒寫成就再回這個 503（不回 `pending`）。
補上之前 daemon 重啟的話，那一筆收成 `unknown`。見 SPEC §6「送出之後結果寫不回去」。

**插隊送出 `send_now: true`**（issue #103，SPEC §6.3 第 9 點）：對方回合中時打斷它，而不是回 409。daemon 照舊寫 turn／訊息，
但不等 idle——打字進 pane 之後按 claude 2.1.275 的 send-now 鍵（`ctrl+x ctrl+s`），**那顆鍵確定生效之後**才把被打斷的那一回合收成 `failed`
（加一則 system 訊息「被插隊送出打斷（claude send-now）」），不留永遠 `in_flight` 的回合，也不在鍵沒生效時假裝打斷了。200 多一個欄位：

| `send_now` | 意思 |
|---|---|
| `"interrupted"` | 真的打斷了一個進行中的回合：send-now 鍵確定生效（herdr 收下，或 transcript 證明送出了） |
| `"idle"` | 當下沒有回合在飛，照一般 Enter 送出（不需要插隊，也不檢查版本） |
| `"not_sent"` | send-now 鍵沒有生效（打字沒回應、打完框是空的、herdr 拒收那顆鍵）：進行中的回合照常，這一則 `delivery:"failed"`，字可能還留在終端的輸入框 |
| `"unknown"` | 不知道 send-now 鍵有沒有生效：進行中的回合**不收**、等證據，這一則 `delivery:"unknown"`（turn 本身是 failed，不佔 in-flight） |

送出鍵之前就被擋下（框裡有字、讀不到、herdr 拒收打字 `pane_send_refused`）→ 可重試的 `409 {"sent":false}`、不留任何列，進行中的回合不動。
送出鍵生效但狀態寫不進去 → `503 {"error":"send_now_state_uncommitted","turn_id","interrupted_turn_id","sent":true,"retryable":true}`：
daemon 自己補，用同一個 `client_request_id` 重送拿到的是這一則、不會再打一次。見 SPEC §6.3 第 9 點。

**前提**：`kind = claude` 且這個 run **跑著的** claude ≥ 2.1.275（`runs.status_json.version`，statusLine 回報的；磁碟上已更新但還沒重啟
不算）。不合格時 daemon **一個鍵都不按**，照原本的路走——AGM 派工排隊、使用者 409——409 的 body 多兩個欄位說明為什麼沒插隊：

| `send_now_refused` | 意思 |
|---|---|
| `send_now_unsupported_kind` | 只有 claude 有這顆鍵；codex／grok 照舊排隊 |
| `send_now_cli_too_old` | 這個 run 跑的 claude 比 2.1.275 舊，重啟套用新版後才能插隊 |
| `send_now_version_unknown` | statusLine 還沒回報版本，不賭那顆鍵 |

`send_now_message` 是同一件事的中文說明，前端直接顯示。灰字（sent／queued 到模型收到之前）由 CLI 自己畫，daemon 不模擬。

**bot 沒在跑時送出 `start_if_stopped: true`**（issue #122，SPEC §4.4a「bot 沒在跑時送出」）：沒有 active run（或 run 還在 `starting`）時，
daemon 在 bot 鎖裡把 queued turn（`awaits_start:1`）＋user 訊息＋附件綁定寫進同一個交易，commit 就回 `200 {"delivery":"queued"}`，
之後才在背景替它啟動（睡著的走 `--resume` 叫醒），起來、閒下來由佇列送出。同一個 `client_request_id` 再送回同一筆（還在等、沒人在起它時順便再起一次）。
bot 在跑就跟沒帶一樣。維護窗口開著時 409 `maintenance_window`、什麼都不寫；已經有一筆在排 → 409 `a turn is already queued for this bot`；
子 agent 不歸 daemon 啟動，照一般的路回 409 `bot has no active run`。啟動失敗或 run 起來後又結束：turn 留在佇列，`start_error` 寫原因（見下）；
使用者自己按停止才撤回。取消用 `POST /api/turns/{id}/withdraw`（已經送出去的回 409）。
active Run 的 herdr session 已不可用 → 寫入 Turn 前回 502，不留 Turn 或 user message。
排隊中的 prompt 是 `status = "queued"` 的 Turn（每個對話最多一筆，`state.bots[].queued_turn`），daemon 在前一回合結束後（或 bot 起來、閒下來時）送出。
turn JSON 帶 `awaits_start`（1＝bot 沒在跑時收下、在等它起來，issue #122）與 `start_error`（上一次替它啟動失敗、或 run 起來後又結束的原因；`null`＝沒有或正在重試）。
**回合中會排隊的只有 AGM 的派工**（2026-09-16）：`supervisor` 派工遇到 in-flight 會建一筆 queued（交辦記成 `delivery="queued"`；同一個 `client_request_id` 重問一筆還在排隊的，回應照樣是 `delivery:"queued"`，不是 turn 欄位上的 `pending`），排超過
`[supervisor] assignment_queue_wait_secs`（預設 30 分鐘）沒送出就撤回那則 queued、把交辦停在 `blocked`，inbox 推 `assignment_undeliverable`（payload 帶 `revoked_turn_id`）。目標身分沒有額度時（`quota::limit_hit_for_bot`）排著的不送、留在佇列，換身分或額度回來才送；這種撞限在 6 小時內會到期的，保險絲不撤（SPEC §4.4a「目標身分沒額度就不送」，issue #108）。**使用者與 web 的 `POST /api/bots/{id}/prompt` 遇到 in-flight 仍回 409**，由呼叫端重試，daemon 不會替你排隊。

409 `reason`：`bot has no active run`、`run is not running`、`agent is blocked; answer the prompt first`、`a turn is already in flight`、
`a previous turn has unknown delivery; abandon it first`、`needs_login`、`picker_open`、`dialog_open`。後三者 daemon 送之前先讀 pane：

| reason | 畫面 | daemon 的處理 |
|---|---|---|
| `needs_login` | claude 停在「Select login method」（該 `CLAUDE_CONFIG_DIR` 沒登入過） | 不建 turn，回 `{"reason":"needs_login","identity":"cc2","message"}`，對話插 system 訊息說明怎麼登入 |
| `picker_open` | codex 的 `/model` 選單開著（字會變成選單操作，Enter 會換模型） | 先 Esc 到真的關掉（`esc` 只退一層）再送；關不掉才回 `{"reason":"picker_open","run_id","message"}` + system 訊息 |
| `dialog_open` | claude 的「Switch model?」確認框（herdr 判成 idle；Enter 會替使用者按 Yes） | 按 Esc（No, go back）再送；退不掉才回 409 + system 訊息 |

**排隊中的 prompt 送出時也過這三道**：claim 之後、`agent.prompt` 之前中了就把 turn 放回 `queued`（清 `run_id`）並插同一則 system 訊息，等下一個 idle 再試。

**打字送出的 run（2026-09-14，SPEC §4.4a）**：daemon 對 pane 直接打過字、或 herdr 沒綁到 agent session 的 run，
prompt 改成打字進 pane 並以無損證據確認。**一個字都沒打時不建立 turn**，回：

| 狀態 | body | 意思 |
|---|---|---|
| 409 | `{"reason":"composer_busy"|"composer_unreadable"|"transcript_not_ready"|"transcript_unreadable"|"codex_log_not_ready"|"no_pane_to_type_into"|"host_unreadable","retryable":true,"sent":false,"run_id"}` | 暫時送不了（輸入框有字、claude 還沒回報 session…）。同一個 `client_request_id` 稍後重送即可；AGM 交辦維持 queued 退避重試。 |
| 422 | `{"error":"delivery_unprovable","reason":"prompt_too_long_to_prove","sent":false,"run_id"}` | 超過 20 萬字，不打。 |
| 409 | `{"reason":"maintenance_window","resource":"restart","held_by","fence","expires_at","retry_after_secs","retryable":true,"sent":false,"message"}` | 有人握著會中斷 pane 的維護窗口（SPEC §18.10），daemon 這一側不送新的 prompt，連 turn 都不建。窗口 release 或到期就自動恢復，同一個 `client_request_id` 原樣重送即可。AGM 派工不走這個 409——它在 `controller::dispatch` 就被 hold 在佇列裡。 |
| 503 | `{"reason":"maintenance_state_unavailable","resource":"restart","retry_after_secs","retryable":true,"sent":false,"message"}`（帶 `Retry-After` header） | **讀不到**維護窗口的狀態（DB 出錯、租約那一列解不開、沒放掉卻沒有讀得懂的到期時間，issue #127）——觀測不到租約不等於沒有租約，所以照窗口處理：不送任何字、連 turn 都不留（已 commit 的那一筆會撤回）。跟上一列 409 `maintenance_window`（**確定**有人握著）分得開；DB 一恢復就重新判斷，同一個 `client_request_id` 原樣重送即可。AGM 派工不回這個 503——留在佇列。 |
| 409 | `{"reason":"resume_unverified","run_id","session_id","retry_after_s"}` | 這個 run 是 `--resume` 接回來的 claude，還沒收到它回報 session（SessionStart hook），不知道接回的是不是原本那段對話（issue #92，SPEC §6.5.2 第 4 點）。連 turn 都不建；回報一到、或等滿 120 秒（刻意放行並在對話插說明）就恢復，同一個 `client_request_id` 原樣重送即可。AGM 派工不回這個 409——排進佇列等驗證。 |

turn 已經建好、還沒打第一個字時 run 就結束（`mark_run_exited` 把它標 failed 並插「run ended」說明）：撤回撤不掉，這時
**不刪任何東西**，回 `200` 那筆 turn 的現況（訊息與說明都留著），跟用同一個 `client_request_id` 重送拿到的回應一致，不回可重試的 409。

沒有無損證據可用的 run（grok、遠端主機、codex 還沒回報 session 的多行 prompt…，矩陣見 SPEC §4.4a）**照樣送出**，回
`200 {"delivery":"unverified"}`：已打字、框收下並在 Enter 後清空，但無法逐字核對。走 herdr `agent.prompt` 的那條路同樣回
`unverified`——它回 ok 但不保證字進得去，沒有證據就是沒有證據。turn JSON 帶 `delivery:"ok"` 與 `delivery_verified:0`，UI 標「未驗證送達」。
turn JSON 同時帶 `delivery_verified` 與 `auto_resend`，前端要靠兩個一起判斷（只有「無證據且不重送」才標「未驗證送達」）。
**「未驗證」不等於「不重送」**（AGM 2026-09-16）：要不要自動重送看另一欄 `auto_resend`——打過字證不明的是 0（重送會重複派工），
`agent.prompt` 是 1（沒送進去才會重送）。按過鍵、該有證據卻證明不了，才是 `200 {"delivery":"unknown"}`。
排隊中的 prompt 遇到 409 類原因放回 `queued`，以 15 秒起、每次加倍、上限 5 分鐘的退避重試（每顆 bot 同時只有一個重試
timer；次數與下次時間存在 turn 上，重啟與其他喚醒都不會提前花掉額度），放回 12 次仍送不出就標 failed 並插 system 訊息；daemon 重啟後依 `next_flush_at` 為每顆 bot 重建一個重試 timer；
codex 的 rollout 還沒寫出來時先放回等 3 次（只算這個原因，綁 run 與 session，換了就重算），之後照樣送出並標 `unverified`；
打字前讀不到 pane（含 herdr 讀取失敗）一律 409 `composer_unreadable`、排隊的放回，不回 502；herdr 不支援 `format=ansi` 時改用純文字讀法；
422 類原因直接標 failed 並插 system 訊息（狀態與說明同一個 transaction）。規劃後、打字前才發現送不出而剛建的 turn 收不回來時，
回 502（不是 409），避免以同一個 request id 重送卻只拿到 failed turn。

## 6. 讀訊息

`GET /api/bots/{id}/messages?before=<message_id>&limit=100`：以插入順序倒序分頁（`before` = 目前最舊一則的 `id`），回傳的 `messages` 已依時間正序。可選 `turn_id` 限定該回合、`role=user|assistant|system` 限定角色，均在該 bot 的 conversation 內過濾後才分頁；不存在或其他 bot 的回合回空訊息清單，非法 role 回 400。不帶篩選參數沿用原行為。
沒有這個 bot → 404；已刪除的 bot 仍讀得到歷史。

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
      "relay_from": null,
      "attachments": [],
      "created_at": "2026-09-05T15:31:00.000Z",
      "updated_at": null
    }
  ],
  "turns": [
    {
      "id": "01M1...", "conversation_id": "01M1...", "run_id": "01M1...",
      "origin": "web" | "external",
      "status": "queued" | "in_flight" | "completed" | "completed_fallback" | "failed",
      "delivery": "pending" | "ok" | "unknown" | "failed",
      "client_request_id": "...", "native_session_id": null, "native_turn_id": null,
      "created_at": "...", "completed_at": null
    }
  ],
  "has_more": false
}
```

`turns` 為最近 `limit+1` 筆（時間倒序），用來判斷回合是否還在跑與 delivery 警示。
UI 標籤：`hook` 不標；`terminal_fallback` 或 `incomplete = 1` 標「終端備援 · 可能不完整」；`system` 灰字系統列。

## 7. 終端快照

`GET /api/bots/{id}/terminal?source=visible&lines=200`：`source ∈ visible | recent | recent_unwrapped | detection`（預設 `visible`），`lines` 1–2000。無 active Run → 404。

```json
{ "bot_id": "01M1...", "run_id": "01M1...", "pane_id": "w1:p2", "source": "visible", "text": "……已去 ANSI……", "revision": 42, "truncated": false, "agent_status": "blocked" }
```

前端在 `lamp = "blocked"` 時每秒輪詢，按鍵打 `/api/bots/{id}/keys`。

## 8. WebSocket `/ws`

`ws://127.0.0.1:7788/ws?token=<token>`，重連帶 `&since=<最後收到的 seq>`。每則一行 `{ "seq": 12, "type": "bot_status", "data": { ... } }`。
`seq` 從 1 遞增（daemon 重啟歸零），保留最近 200 則；補不齊或 seq 倒退會先送不帶 data 的 `{ "type": "resync", "seq": 12 }`，收到就重新 `GET /api/state` 與訊息。

| type | data |
|---|---|
| `bot_status` | `{"bot_id", "host":"local"\|"<name>", "run": <run 物件或 null>, "connected"}`（`connected` 是該 bot 所屬 host 的連線狀態） |
| `message_added` | `{"bot_id", "message": <message 物件>}` |
| `turn_updated` | `{"bot_id", "turn": <turn 物件>}` |
| `turn_progress` | 即時輸出，見「WS `turn_progress`」 |
| `project_changed` | `{"project_id"}`（或 `{}`） |
| `bot_changed` | `{"bot_id"}` |
| `daemon_status` | `{"herdr_connected", "hosts": {"<name>": {"connected","error"?}}}` |
| `host_changed` | `{"name","connected","error"?}` |
| `quota_updated` | 見 §12.5 |
| `mem_updated` | 與 `GET /api/mem` 同形 |
| `bots_restart_progress` / `bots_restart_done` | 見 §10.3a |
| `supervisor_health` | 見「總管」一節 |

終端畫面不走 WS，輪詢 §7。

## 9. 前端流程

1. `GET /api/session` 取 token（只存記憶體）。
2. `GET /api/state` 畫 sidebar。
3. 開 `/ws`：`bot_status` 更新燈號，`message_added` / `turn_updated` 更新聊天；`project_changed` / `bot_changed` / `resync` → 重拉 state。
4. 選 bot 時 `GET /api/bots/{id}/messages?limit=100`。
5. `POST /api/bots/{id}/prompt`（自帶 `client_request_id`）；user 氣泡經 `message_added` 推回，用 `message.id` 去重，不做本地暫存氣泡。

## 目錄瀏覽 `GET /api/fs/dirs?host=&path=&hidden=`

新增 Project 的目錄選擇器。`path` 空白為家目錄，支援 `~`；只列子目錄（含指向目錄的 symlink）。`.` 開頭預設略過，`hidden=1|true|yes` 才列。`host` 省略 = 本機，遠端見 SPEC §11.5。

```json
{"path":"/Users/me/project","parent":"/Users/me","home":"/Users/me","entries":[{"name":"foo","path":"/Users/me/project/foo","git":true}]}
```

路徑不存在或不是目錄 → 400。

### `POST /api/bots/{id}/read`
跨裝置共用的已讀位置（2026-09-15）。body `{"at":"<讀到的最後一則 created_at>","message_id":"<那則 id>"}`，兩者可省（`at` 省略＝現在）；標記**只往前推**，較舊的送來不會倒退。
回 `{"bot_id","read_mark":{"at","id"},"unread"}`，並推 WS `bot_read`（同形）讓其他分頁／裝置重拉 state。`at` 不是 RFC 3339 → 400；bot 不存在 404。有效時間先轉為 UTC 毫秒 `Z` 格式，晚於 daemon 現在的值夾到現在，避免裝置時鐘錯誤永久遮住新訊息；回傳 `read_mark.at` 為正規化後的值。
`GET /api/state` 每顆 bot 帶 `unread`（標記之後的 assistant 訊息依回合去重的數目；沒有標記＝全部）與 `read_mark`（`{at,id}` 或 `null`）。升級建表時既有 bot 的標記設為當下，舊訊息不算未讀。

### `GET /api/bots/{id}/local-image?path=<路徑>`
對話 Markdown 裡的本機圖片（`![](docs/shot.png)`、`/Users/…/x.png`、`file://…`）。相對路徑先以該 bot 的工作目錄（`bots.cwd`，child 的 worktree）為底、那裡沒有再退回**專案目錄**；
字面路徑讀不到時再試 `%XX` 解碼後的（Markdown 渲染會把中文檔名、空白編碼）。**允許範圍一律是專案目錄**：符號連結解開後仍須在專案目錄內，副檔名限 `png/jpg/jpeg/gif/webp`（不含 svg），≤ 20 MiB。回圖片位元組與對應 `Content-Type`。
專案外、非圖片、不存在、太大、遠端主機的專案一律 `404 {"what":"image"}`；缺 `path` 400；bot 不存在 404。前端讀不到就把路徑寫成文字，不畫破圖。

### `GET /api/bots/{id}/outbox`
這顆 bot 交給使用者的檔案（SPEC §6.5f，2026-09-16 使用者裁示取代 scratchpad）。bot 把要給使用者的檔案放進 `$AM_OUTBOX`（`<data_dir>/outbox/<bot_id>/`），前端「檔案暫存」下半段列出來讓人下載。**不讀 scratchpad。**

```json
{"dir":"/Users/me/.config/agents-manager/outbox/01M1…","ttl_secs":3600,
 "files":[{"name":"tracking.tsv","size":18432,"modified":1789600000,"expires_at":1789603600,"remaining_secs":2520}]}
```

- 只列**第一層的一般檔案**（不遞迴；子目錄與符號連結不列），新的排前面，最多 300 筆。
- `expires_at` = mtime + `ttl_secs`；`remaining_secs` 是回應當下還剩幾秒，到期是 0（AGM 的 `com.agm.outbox-gc` 每 10 分鐘才清一次，0 的檔案還會出現一下）。前端從回應那一刻往下扣，不拿瀏覽器時鐘比 `expires_at`。
- **一律不列**：隱藏檔、資料庫與旁檔（檔名含 `.sqlite`，或 `.db` 結尾／`.db-`／`.db.`）、金鑰與憑證（`.pem` `.key` `.p12` `.pfx` `.jks` `.keystore` `.ppk` `.kdbx` `.env` `.token` `.keychain`、`id_rsa*` 等，以及 `auth.json`、`credentials.json`、`application_default_credentials.json`、`hosts.yml`、`ui-token`），以及檔頭是 `SQLite format 3` 或 PEM 私鑰的檔案。規則本來就禁止放這些，這是第二道。
- 目錄不存在（還沒寫過、被清理收掉）→ `200` 空清單。遠端主機的 bot → `200 {"files":[],"ttl_secs":3600,"reason":"outbox_remote"}`。bot 不存在 404。
- `outbox` 或 `<bot_id>` 這兩段是符號連結、或擁有者跟資料目錄不同 → `200 {"files":[],"ttl_secs":3600,"reason":"outbox_untrusted"}`，下載 404：界線不能跟著連結搬到別處（例如 `~/.codex`）。

### `GET /api/bots/{id}/outbox/file?path=<檔名>`
下載 outbox 裡的一個檔案。`path` 解開符號連結後必須仍在該 bot 的 outbox 內、是一般檔案、路徑上沒有隱藏目錄、也不是上面「一律不列」的那幾類，否則 `404 {"what":"file"}`（指到 scratchpad 的絕對路徑或符號連結一樣 404）；缺 `path` 400；bot 不存在 404；遠端主機的 bot 409 `outbox_remote`；大於 64 MiB 409 `file_too_large`。

一律 `Content-Disposition: attachment`（檔名走 `filename` + RFC 5987 `filename*`），加 `X-Content-Type-Options: nosniff` 與 `Cache-Control: private, no-store`。`Content-Type` 只認白名單（文字/JSON/CSV/TSV/PNG/JPEG/GIF/WebP/PDF），其餘一律 `application/octet-stream`。只讀，沒有刪除或覆寫的端點。

### `GET /api/bots/{id}/scratchpad`、`GET /api/bots/{id}/scratchpad/file`（已移除）
2026-09-16 起一律 `404 {"what":"scratchpad"}`：scratchpad 不再給使用者（它暴露過私鑰與正式 DB 複本）。明確回 404，不落到 SPA fallback 回 HTML。

## 非 agent 的 pane（SPEC §6.5e）

### `GET /api/projects/{id}/panes`
這個專案 host 上、**不是 agent** 且屬於這個專案的 pane。**沒歸屬的不在這裡**（它不屬於任何專案，掛在每個專案底下會重複出現）：走 `GET /api/panes?unowned=1`。
```json
{ "project_id":"01M1…", "host":"local",
  "panes":[{"pane_id":"w168:p62","host":"local","workspace_id":"w168","tab_id":"t39","cwd":"/Users/m4p/…/wt",
            "kind":"service","owner_bot_id":"01M1…","project_id":"01M1…","purpose":"dev-server",
            "foreground":"node next dev","listen_ports":[3010],"read_only":true,
            "last_output_at":"2026-09-16T07:10:00.000Z","first_seen":"…","last_seen":"…","gc_optin":false,
            "owned_by":"bot","label":null,"scratch":false,"orphaned":false}] }
```
- `kind`：`service`（有前景程式或 listen port）／`shell`（只有 shell）。agent pane 不在這裡。**不是權限**。
- `read_only`：本機＝`listen_ports` 非空（`kind=service` 但沒有 port 的，例如跑著 vim，照樣可以打字）；遠端不算 port，
  ＝`kind=service` 或記過 port。跟打字那一端（`/hosts/{name}/shells/{pane_id}/text|keys`）同一條規則。
- `owner_bot_id`／`owned_by`：`bot`＝從 pane 行程樹的 `AM_BOT_ID` 推斷；`user`＝沒有標記但 cwd 對得到這個專案（列在專案底下，**預設仍不自動關**）；`none`＝連專案都對不到。
- `listen_ports` 只在本機判斷，遠端一律空陣列。`last_output_at` 由 herdr 的 `revision` 變化推進，不讀畫面內容。
- Project 不存在 404。

### `POST /relay/pane`（表單，bot 專用）
`bot_id`／`pane_id`／`purpose`，header `X-AM-Bot-Token`；shim 開完 pane 後自己呼叫（`herdr … --purpose <文字>`）。
只記用途：pane 還沒被掃到就先建一列，**已經有 owner 的不會被改寫**（歸屬永遠由掃描時的 `AM_BOT_ID` 決定）。
token 不對 401；其他失敗照樣回 200（`recorded:false`），少一個用途字串不該讓 bot 開 pane 失敗。

### `GET /api/panes?unowned=1`
全機的非 agent pane（列的形狀同上）；`unowned=1` 只回 `owned_by="none"`（連 cwd 都對不到專案）的那些，同一台的 scratch 排第一。
- `scratch: true`：這台那顆固定的 scratch（daemon 選的，規則見 SPEC §6.5e；前端不要自己重算）。其餘是「多出來的」。
- `orphaned: true`：綁過的專案或擁有它的 bot 已經刪掉（推過 `pane_orphaned`），不會是 scratch。
- `label`：herdr 上的 pane 名字；scratch 選中時會被改成 `[panes] scratch_name`。

### `POST /api/panes/{id}/focus?host=local`
把 herdr 的焦點切到那顆 pane，只動焦點。主機沒連上 502。

### `POST /api/panes/{id}/adopt?host=local`
`{"owner_bot_id"?, "purpose"?, "allow_gc"?}` → 補歸屬與用途。省略的欄位不動。
**使用者手開的 pane 不會因為 adopt 就變成可自動關**，除非帶 `allow_gc: true`（寫進 `gc_optin`，並記 log）。
adopt 之後孤兒通知標記會清掉。pane 不存在 404、`owner_bot_id` 不存在 404。

### `POST /api/panes/{id}/close?host=local[&confirm=true]`
關掉那個 pane（空了的 tab 一併收）。表上的 `kind` 是掃描的快取，所以關之前即時再看一次：
- 有 active run、或 herdr 說裡面現在有 agent → `403 {"error":"agent_pane"}`（帶 confirm 也一樣；agent 走 bot 的 stop）。
- herdr 說 pane 已經不在 → 刪掉那一列，`404 {"what":"pane"}`。
- 即時的分類是 `service`，或讀不到事實 → 沒帶 `confirm=true` 就 `409 {"reason":"service_pane","pane":{…},"unverified":bool}`，
  `pane` 的 `kind`／`foreground`／`listen_ports`／`read_only` 換成即時值（`unverified:true`＝讀不到，沿用表上的）——UI 要先把 port 顯示給人看再問一次。
- 表裡沒有這顆 404；主機沒連上 502。

## 請 AGM 解析 claude 新版（使用者 2026-09-19）

### `GET /api/claude-update/review`
這一版的 AGM 解析到哪了——更新框一打開就讀，**有結論就直接印在框裡**（使用者 2026-09-19：不要只給一句
「結論會回到這裡」）。`?host=`／`?to=` 可指定，預設本機與磁碟上那一版。

```json
{"version":"2.1.277","review":{"state":"done","assignment_id":"01…","target_bot_name":"AGM-responder",
 "asked_at":"…","answered_at":"…","result":"2.1.277 沒有值得跟進的東西…"}}
```

`state`：`none`（還沒派，UI 顯示按鈕）｜`pending`（派了還沒結論）｜`done`（`result` 就是結論原文）。
只有空白的回覆算 `pending`——對方還沒講東西不該顯示成有結論。

結論從**收件匣事件的回合**讀（`supervisor_inbox.notify_turn_id` → 那個回合的最後一則 assistant 訊息），
不是從 assignment：派給 AGM 角色（協調者／巡檢）的工作一律走交接佇列，那條路**不會產生 assignment**，
`result` 永遠是空的（2026-09-19 上線後實測，視窗一直停在「還沒派」）。按鈕派的（`…-ui`）與 kick 派的
（不帶 `-ui`）兩個 crid 都查，同一版誰先派結論都算數。

### `POST /api/claude-update/review`
`{host?, from?, to?}`（預設 `host=local`、`to`＝磁碟上那一版）。把這一版的 changelog 組成交辦派給**協調者**，
唯讀——只建交辦，不 build、不重啟、不碰 claude 的檔案。內容與 `scripts/ops/claude-release-task.md` 同一套規則，
結論照那份任務的指示回到使用者入口。

```json
{"version":"2.1.277","from_version":"2.1.276","target_bot_id":"01…","target_bot_name":"AGM-responder",
 "assignment_id":"01…","duplicate":false,"sections":1}
```

- **正文只有一份來源**：`claude-release-task.md`（AGM 目錄裝好的那份優先，其次 repo 的 `scripts/ops/`）原文
  ＋ 版本尾段（舊／新版號與兩顆 binary 路徑），跟 `claude-release-kick.sh` 完全一樣；規則要改就改那個檔案。
  找不到那份檔案 → 409 `no_task_file`；
- 按鈕用**自己的** `client_request_id`：`agm-claude-release-<版本>-ui`（kick 用不帶 `-ui` 的那個）。
  分開是因為 kick 走 AGM 收件匣的 `bot_request`，那條路沒有 assignment，**結論無處可讀**；走自己的交辦
  才有 `result` 能回填到更新框。同一版重按仍然只有一筆，回應帶 `duplicate:true` 與當下的 `review` 狀態。
- （舊行為，仍保留在回應裡）`client_request_id` 曾與 kick 共用 `agm-claude-release-<版本>`。同一版已經派過時**在送出之前**
  就回 `duplicate:true`——**兩個地方都查**：既有的 assignment（回 `assignment_id`），以及 AGM 收件匣裡同一個
  crid 的 `bot_request`（回 `inbox_event_id`；kick 是走收件匣派的，那一步還沒有 assignment，不查就會撞上
  `bot_requests` 的 409 `request_mismatch`——2026-09-19 上線後實測）（UI 顯示「已經派過」，不是錯誤）——kick 與按鈕的正文差一句觸發來源，
  不先查會撞上 `text_mismatch` 409；
- 派給誰：`AGM_RELEASE_BOT` ＞ `runtime.json` 的 `release_bot_id` ＞ `responder_bot_id`，**絕不派給巡檢**
  （daemon 擋「總管對自己下交辦」）。都沒設 → 409 `no_target`，`message` 就是給使用者看的原因；
- 那台主機還讀不到 claude 版本 → 409 `no_version`；
- changelog 原文框成引用（反引號數比原文最長那串多一個）並註明是資料，上限 8000 字，超過截斷並註明；
  抓不到 changelog 照樣派，正文請協調者改用 diff binary 的做法。

## 外部 Cargo 主機（issue #104）

把 `check`／`test`／`clippy` 丟到一台 SSH 主機跑；`build`／`run` 留本機（Linux/x86_64 的產物在 macOS/aarch64 上不能用）。

shim 轉遠端要 pane 裡有 `AM_DAEMON_EXE`、`AM_CONFIG_PATH`、`AM_DATA_DIR`（`AM_DAEMON_EXE` 指到的檔案還要能執行）。缺任何一個時
**不再靜默退回本機**：stderr 印一行 `外部編譯沒有啟用：這個 pane 缺 <名字>……這次 <子指令> 在本機跑`，缺哪個講哪個。

遠端工作目錄（issue #141，`remote_cargo.rs`）：`<remote_root>/<worktree 路徑的 fnv1a64>/` 底下，
- `shared/`：同一棵 worktree 共用的原始碼＋`target/`，用完保留，下次 rsync 只傳差異、cargo 增量編譯。旁邊的
  `shared.lock` 是 flock，同一時間只給一次呼叫；被佔著（同一棵 worktree 同時兩個 `cargo test`）時**先等它用完**（每秒再試，最多 5 分鐘，
  issue #104：等完再增量編譯只要十幾秒，冷編譯要重編幾分鐘、還多佔一份遠端 RAM 與 2～3G 磁碟），等超過 5 分鐘才改用
  `job-<pid>-<ms>/`，冷編譯、結束就刪，stderr 都會講。
- 同步、算 hash 的是**工作區根**（issue #177），不是呼叫時的 cwd：像 cargo 一樣往上找最近一層宣告 `[workspace]` 的
  `Cargo.toml`（沒有工作區就用最近的套件根；完全不在 cargo 專案裡才是 cwd 本身），遠端 cargo 再 `cd` 到同一個相對子目錄。
  所以 `cd daemon && cargo check` 與在根目錄呼叫共用同一份 `shared/`，遠端有根的 `Cargo.lock`／`[profile]`／`.cargo/config.toml`，
  不會重新解析依賴。
- 每次呼叫先開一條守門 ssh 拿鎖，再 rsync、再跑 cargo；守門讀 stdin 等到 EOF 才清理，所以 helper 成功、失敗、
  被 Ctrl-C／SIGTERM／kill -9 都會清（遠端還在跑的 cargo 按 process group 收掉，`job-*` 刪掉，`shared` 解鎖）。
- **helper 被砍時遠端也要停（issue #201）**：SIGINT（Ctrl-C）、SIGTERM、SIGHUP 不再直接把 helper 打死（那會留下還連著的 ssh 與遠端還在跑的 cargo）——helper 記下訊號、
  把本機的 rsync／ssh 收掉、等守門把遠端那組行程收乾淨（process group，同 #183）並還目錄與鎖，才用 `128＋訊號` 結束；第二次訊號直接結束（遠端照樣清得掉）。
  SIGKILL 攔不到，只能靠連線中斷偵測：kernel 收掉 helper 的 fd，守門那條 ssh 的 stdin 關掉，守門讀到 EOF 就收尾。守門收尾時，除了按 process group 收，
  還把 cwd 在這個目錄裡、卻離開了 process group（自己 `setsid` 的輔助行程）的一併收掉。
- 租約身分（fencing token，issue #148）是本機每次呼叫新產生的 128 位元隨機數（32 個小寫 hex），不是遠端 shell 的 PID
  （PID 會被回收重用：值一樣不等於同一次租約）。守門把它寫進 `<dir>.owner`、原樣回給 helper，run 帶著它核對 owner
  （對不上＝晚到的舊 run，exit 126 不跑 cargo）；`<dir>.pgid-<token>` 也以它命名，守門只收自己那一代的 process group。
  token 只有固定格式，不可能夾帶 shell 字元或路徑。
- 守門順手回收孤兒：沒鎖被持有、沒有行程的 cwd 在裡面、閒置超過 10 分鐘的 `job-*`／舊版 `<pid>/`；閒置超過
  `[build.remote] shared_idle_hours`（預設 3）小時的 `shared/`（遠端磁碟剩不到 25% 時也降到 10 分鐘）。只碰 `<16 位 hex>/<shared｜job-*｜數字>` 這種名字。
  **`shared/` 另有數量上限（issue #196）**：`[build.remote] max_shared_dirs`（預設 8，`0`＝不限）——一天開十幾顆子 agent、每張票數個變異副本，每個路徑一個新的 hash、
  各 2～3G（實測 36 個 hash、29G），只靠時間擋不住。超過就從最久沒用的開始收（LRU；至少閒置 10 分鐘，剛用完的不動；鎖被持有、有行程在用、這次自己的永遠不收），
  時機是每次 remote-cargo 呼叫開始時（跟 #141 的孤兒回收同一處），不靠 crontab。
- **遠端名額（issue #104）**：同一個 `remote_root` 不分 worktree 同時最多 `[build.remote] max_concurrent` 個遠端編譯（`<remote_root>/.slots/<n>`
  的 flock，守門拿到目錄之後再搶、拿著直到結束，怎麼死都會放）。#155 讓遠端編譯不佔本機名額之後遠端沒有自己的上限，2026-09-19 就是 9 個冷編譯同時跑把遠端壓垮。
  `0`（預設）＝守門依那台的核數與 RAM 算：`min(核數 × 1.5 ÷ cargo_jobs, (MemTotal − 8 GiB) ÷ 8 GiB)`，至少 1——CPU 容許 1.5 倍超賣（一次編譯大多時間只有
  一顆 rustc 在跑），記憶體不超賣（實測一次冷的 `cargo test -p agents-managerd` 光主 crate 那顆 rustc 就多吃約 4.6 GiB）；32 核／64 GiB、`cargo_jobs = 8` 是 6。
  全滿就排隊：每秒再試，stderr 講「遠端同時編譯已滿（N 個），排隊等名額」，排到了講等了幾秒。順序是**先目錄、後名額**：等 shared 的人不佔名額，拿著名額的人不再等任何鎖。
  排隊期間守門每秒看一次 helper 還在不在：helper 那條 ssh 斷了（被砍、Ctrl-C、網路斷）時遠端 sshd 會關掉守門的 **stdin**（stdout 反而照樣寫得進去，
  2026-09-19 用真的 sshd 量過），所以用 `timeout 1 dd` 讀 stdin——讀到 EOF 就自己結束、放掉手上的鎖（實測砍掉本機 ssh 後 1 秒內）；helper 交握前不寫 stdin，不會吃掉資料。
  helper 這端交握改在另一條執行緒讀，收到終止訊號照樣馬上停。排超過 30 分鐘（每個佔名額的編譯都受 `timeout_secs` 管，正常兩輪內就排到；再久＝有名額被卡住，例如守門那條連線半開）就放棄：
  遠端什麼都沒跑，**結束碼 75**（EX_TEMPFAIL，稍後再試；shim 原樣帶出，不退回本機）。
- 遠端要是有 `flock`（util-linux）與 `/proc` 的 Linux，沒有就直接報錯、不跑。
- **整體上限（issue #194）**：一次遠端編譯（同步＋編譯＋測試）最多 `[build.remote] timeout_secs`（預設 **720 秒＝12 分鐘**；實測 112 次遠端編譯最長 8.9 分鐘、
  全套 test 中位數 5.0、P90 8.7）。從**拿到遠端名額與目錄**才開始算（issue #104）：排隊的時間不算，不然滿載時排久一點的編譯一開始跑就被砍。連線正常、遠端的 cargo 或測試卡住（死結、等鎖、測試掛住）時 ssh 的 ConnectTimeout／ServerAlive 管不到，沒有上限 helper 與呼叫端的 agent 會一直等。
  超過就：砍掉本機的 rsync／ssh、守門收到 EOF 把遠端那一整組行程（process group，同 #183）收掉並還目錄與鎖（#141），stderr 印一行
  「遠端編譯超過 12 分鐘上限，已中止（…可調 [build.remote] timeout_secs）」，**結束碼 124**（跟 GNU `timeout` 一樣）。shim 只把 125 當成「退回本機」，
  所以**不會在本機重跑**（那只會讓卡住的東西在本機再卡一次）。`timeout_secs = 0`＝不設上限。
- **測試執行緒上限（issue #202）**：遠端命令帶 `RUST_TEST_THREADS=<[build.remote] test_threads>`（預設 8）。遠端是 32 vCPU 的超賣主機，libtest 預設開 32 個執行緒，大多在系統呼叫與鎖上互搶
  （`sy` 59～64%、每秒 50 萬次 context switch），全套 1611 條測試：32 個執行緒 283 秒、16 個 199 秒、12 個 219 秒、8 個 168～176 秒（預設挑 8）。呼叫端自己帶了 `--test-threads`（命令列旗標本來就優先於環境變數）或設了 `RUST_TEST_THREADS`
  就尊重呼叫端；`0`＝不設。`check`／`clippy` 不受影響（那是 `cargo_jobs` 管的）。
- helper 的每一條 ssh（守門／run／probe／安裝）與 rsync 的 `-e` 都帶 `ConnectTimeout=15`、`ServerAliveInterval=15`、`ServerAliveCountMax=3`
  （issue #174，跟 `hosts.rs` 的連線一致）：連線靜默斷掉（Wi-Fi 換 AP、Mac 睡著醒來 IP 變了、遠端掉電）約 45 秒內就放棄，
  不會卡到 TCP keepalive 的 2 小時。

### `GET /api/build/remote` / `PUT /api/build/remote`
`{enabled, host, user, ssh_port, remote_root, cargo_jobs, test_threads, timeout_secs, shared_idle_hours, max_shared_dirs, max_concurrent, password_set}`。PUT 另收 `password`
（省略＝沿用現在的密碼，換主機也一樣；`""`＝清掉改用 SSH key/agent；其他＝新密碼）——不進 config、不回前端。`host`／`user` 空字串又要 `enabled` → 400。
**設定與密碼一起提交（issue #104）**：新密碼先寫成一份還沒人指到的 `<data-dir>/remote-cargo-password.<16 hex>`（open(2) 時就是 0600，不是事後 chmod；寫完 fsync 才從
`.tmp` rename 成正式檔名），再把 config.toml 一次換成「新設定＋`[build.remote] password_id` 指向新檔」——唯一的提交點是 config.toml 的 rename；最後才刪掉沒有設定指到的舊密碼檔。
任何一步失敗或行程死掉，重讀磁碟只會是「舊設定＋舊密碼」或「新設定＋新密碼」，不會出現新主機配舊密碼（舊密碼被送去新主機）或權限比 0600 寬的密碼檔；
提交前失敗 → 5xx、什麼都沒變。沒有 `password_id` 的舊設定照舊讀 `remote-cargo-password`，下一次儲存就換成新格式；設定指到的密碼檔不見了是錯誤（不悄悄改用 key/agent）。
`max_concurrent`＝遠端名額（見上「遠端名額」）：PUT 省略＝維持現在的值，`0`＝依遠端核數與 RAM 自動算，超過 64 → 400。
`test_threads`＝遠端 `cargo test` 的測試執行緒上限（`RUST_TEST_THREADS`，預設 8，`0`＝不設，最多 256；PUT 省略＝維持現在的值）。
`timeout_secs`＝一次遠端編譯的整體時間上限（見上「整體上限」）：PUT 省略＝維持現在的值，`0`＝不設上限，超過 86400 → 400。
`shared_idle_hours`（至少 1）與 `max_shared_dirs`（`0`＝不限）是遠端 `shared/` 的回收政策（見上），PUT 省略＝維持現在的值。

### `POST /api/build/remote/test`
連上去看一眼：

```json
{"ok":true,"output":"OS=Linux\nARCH=x86_64\nCARGO=/home/ubuntu/.cargo/bin/cargo\ncargo 1.90.0",
 "password_auth":true,"os":"Linux","arch":"x86_64","cargo_path":"…","cargo_version":"cargo 1.90.0","cargo_missing":false}
```

**連得上但沒有 cargo 不是錯誤**：`cargo_missing:true`、`cargo_path:null`，UI 據此提示安裝。ssh 本身失敗才 5xx。
另回 `clippy_version`（`cargo clippy --version`，沒有是 `null`）與 `clippy_missing`（有 cargo 卻沒有 clippy，issue #104）：clippy 是會轉過去的三個指令之一，
rustup 的 minimal profile 不含它——UI 據此提示「按安裝補上」，不說「可用」然後第一次 `cargo clippy` 才失敗。
密碼認證優先用 `sshpass`，沒有就走 ssh 自己的 askpass（`SSH_ASKPASS_REQUIRE=force`，OpenSSH 8.4+）；兩條都不行才 5xx。

### `POST /api/build/remote/install-toolchain`
在那台跑 `rustup`（`--profile minimal --component clippy --no-modify-path`，不動遠端的 shell profile）。冪等：已經有 cargo 就不重裝，
只在缺 clippy 時 `rustup component add clippy`（issue #104；不是 rustup 裝的工具鏈補不上，回 `clippy_missing:true`，UI 當場提醒）。

`200 {"ok":true,"already_installed":bool,"cargo_version":"cargo 1.90.0","cc_missing":bool,"clippy_missing":bool,"output":"…"}`
（`cc_missing:true`＝那台沒有 `cc`／`gcc`：rustup 不裝 linker，`cargo test` 會在連結那一步才失敗，UI 當場提醒裝 build-essential）；遠端沒有 `curl` 或 rustup
失敗 → 5xx 並帶 stderr。第一次大約要一兩分鐘。`~/.cargo/bin` 由 daemon 自己接到遠端 PATH 前面（probe 與真正的
遠端 cargo 都是），所以不必改遠端 profile。

## Build scheduler（全機 cargo/rustc 併發，SPEC §6.5g，issue #90）

不在 `/api` 底下的三支（`acquire`／`renew`／`release`）：bot 的 pane 只有自己的 hook token，拿不到一般 UI token。

### `POST /build-slots/acquire`（表單）
`{holder, bot_id?, purpose?, host?}`。header 二選一：`X-AM-Bot-Token`＋body 的 `bot_id`（驗證那顆 bot 的
`hook_token`），或 `X-AM-Token`（人工 host shell）。都沒有／都不對 → 403。`holder` 空字串 400。

- 拿到：`200 {"granted":true,"token","expires_at","cargo_jobs","lease_ttl_secs"}`。同一個 `holder` 對已經握著、
  沒過期的名額重 call 是幂等的，回同一份憑證。
- 額滿或還沒輪到：`200 {"granted":false,"active","max_concurrent","since","retry_after_secs"}`——**這是正常的等待
  狀態，不是錯誤**，回 200 不是 4xx／5xx；呼叫端照 `retry_after_secs` 再問一次。**FIFO**（SPEC §6.5g）：名額空出
  來時只給排隊排最早的 holder（`since` 最早，同值比 `holder`），就算這一刻剛好也在問、名額也剛好空著一樣要等。

### `POST /build-slots/renew`（表單）
`{holder, token}`。**不驗 bot／UI token**，`token` 本身就是憑證。只有還在 `held` 且沒過期的名額能續：
`200 {"renewed":true,"expires_at"}`；找不到這一列（沒拿過／已過期被收回）→ `404`；`token` 不對 → `403 {"error":"token_mismatch"}`。
cargo shim 把這兩個明確的拒絕（`not_found`／`token_mismatch`）視為**名額已失去**，立刻停掉前景的 cargo 行程樹、退 75（issue #128，SPEC §6.5g）；
其他失敗（連不上、5xx）在到期前一直重試，撐到保守估的 deadline 仍續不上也停。

### `POST /build-slots/release`（表單）
`{holder, token}`。一律幂等，永遠 `200 {"released":true}`（找不到、已過期、token 不對都當作「已經不是你的事了」）。

### `GET /api/build-slots`
一般 `X-AM-Token`。現況（UI／人工查用）：

```json
{
  "max_concurrent": 2,
  "cargo_jobs": 2,
  "lease_ttl_secs": 180,
  "active": 1,
  "slots": [
    {"holder": "proj-abc-review:12345", "status": "held", "bot_id": "01M...", "purpose": "test -p agents-managerd",
     "host": "local", "since": "2026-09-18T03:00:00.000Z", "last_seen": "2026-09-18T03:01:00.000Z",
     "expires_at": "2026-09-18T03:04:00.000Z"},
    {"holder": "manual:host:987", "status": "waiting", "bot_id": null, "purpose": "build",
     "host": "local", "since": "2026-09-18T03:00:30.000Z", "last_seen": "2026-09-18T03:00:55.000Z", "expires_at": null}
  ]
}
```

## 記憶體

### `GET /api/mem`
herdr 進程樹佔多少常駐記憶體（SPEC §15）。

```json
{"total_bytes":1610612736,"herdr_bytes":50331648,"agents_bytes":1560281088,"processes":5,
 "projects":[{"project_id":"p1","host":"local","panes":3,"bytes":1932735283}],
 "hosts":[{"host":"local","herdr_bytes":50331648,"agents_bytes":1560281088,"total_bytes":1610612736,"processes":5,"error":null,
           "browsers":[{"name":"Chrome","tabs":34,"bytes":3435973836,"processes":41}],
           "machine":{"total_bytes":17179869184,"available_bytes":5536579584}},
          {"host":"m4p","herdr_bytes":0,"agents_bytes":0,"total_bytes":0,"processes":0,"error":"未連線","browsers":[],"machine":null}]}
```

- `browsers`：同一份 `ps` 裡的 Chromium 系瀏覽器依 app bundle 分組（`Google Chrome.app` → `Chrome`、`ego lite.app` → `ego`），`tabs` = `--type=renderer` 數。不算進 `total_bytes`；前端總分頁 ≥ 30 時標紅。
- `machine`：整台機器（SPEC §15.1a），認不出來 `null`。
- `projects`（2026-09-15）：`[{"project_id","host","panes","bytes"}]`，側欄專案標題的「N pane · RAM」。只算 herdr 樹裡帶 `AM_BOT_ID` 的程序（該專案的 bot 與 child；每個程序算自己的 RSS 一次），`panes` 以 socket＋pane id 去重；bot 已刪或量不到的主機不列，沒有程序的專案不在清單裡。多一趟帶環境變數的 `ps`（同 `/mem/processes`）；pane 數變了或任一專案差 ≥ 1 MiB 也會推 `mem_updated`。
- 量不到的主機用 `error` 回報，不從清單消失（UI 標星號）。每 15 秒取樣，變化超過 1 MiB 才推 `mem_updated`。

### `GET /api/mem/processes?host=local`
`host` 省略 = `local`，不認得 404。

```json
{"host":"local","sampled_at":"2026-09-08T04:11:02Z","processes":[
  {"pid":59407,"ppid":37845,"rss_bytes":412000000,"exe":"claude","argv":"claude --dangerously-skip-permissions",
   "pane_id":"w168:p1","socket_path":"/Users/me/.config/herdr/herdr.sock","bot_id":"b3","bot_name":"opus","project_id":"p1",
   "owner":"bot","subtree_bytes":420000000,"children":3}]}
```

- `owner`：`bot`（有 `AM_BOT_ID`）/ `pane`（只有 `HERDR_PANE_ID`）/ `herdr`（不列）/ `unknown`。bot 已刪仍回 `bot_id`。
- `subtree_bytes` = 自己 + 子孫 RSS，清單依它降冪。只列 `claude`/`codex`/`grok`/`node`/`bash`/`zsh`/`sh`/`fish` 且 ≥ 8 MiB 的，其餘併進父程序。

### `POST /api/mem/processes/kill`
`{"host":"local","pid":59407,"signal":"TERM"}`（`signal` 只認 `TERM` / `KILL`，預設 TERM）→ `{"host","pid","signal","exe","freed_bytes"}`，並立刻推一次 `mem_updated`。
送訊號前重新取樣判定：不在該主機 herdr 樹裡 400；就是 herdr 400；`owner == "bot"` → `409 {"reason":"bot_process","bot_id","message"}`（走 `POST /bots/{id}/stop`）。

### `GET /api/mem/processes/pane?host=local&pane_id=wM:pB&socket=<socket_path>&lines=40`
回那個 pane 現在畫面的字（`lines` 1–500），形狀同主機 shell 的 terminal，`source` 固定 `visible`，**不要求 pane 是 AG Man 開的**。只讀。
`socket` 是清單那列的 `socket_path`（pane id 是 per session 的；只限本機，遠端給 `socket` 回 400），省略時走該主機設定的 session。

```json
{ "host":"local","pane_id":"wM:pB","source":"visible","text":"…","revision":12,"truncated":false,"columns":185,"rows":54 }
```

主機沒接上 herdr 502；缺 `pane_id` 400；pane 不存在 502。

## 遠端主機 hosts（SPEC §11）

Project 可在另一台機器，daemon 透過 SSH 轉發連遠端 herdr。`host` 是 host 名稱，`"local"` 保留給本機。

### `GET /api/state` 的 host 欄位

```json
{
  "hosts": [
    {"name":"local","ssh":null,"ssh_port":null,"ssh_opts":[],"herdr_session":"agents-manager","remote_path":null,"connected":true,"error":null},
    {"name":"m4p","ssh":"m4p@100.112.229.82","ssh_port":22,"ssh_opts":[],"herdr_session":"agents-manager",
     "remote_path":"/opt/homebrew/bin:$HOME/.local/bin","connected":false,"error":"ssh master exited immediately (exit status: 255)"}
  ],
  "projects": [ {"id":"01M1...","path":"/Users/m4p/work/foo","label":"foo@m4p","host":"m4p","workspace_id":null,"bots":[ ... ]} ]
}
```

- `hosts` 必含 `local` 且排第一；`local` 的 `connected` = 本機 herdr 連線（同頂層 `connected`）。`error` 為 `null` 或人類可讀字串。
- `default_connected`：本機使用者 Herdr `default` session 的連線狀態；採用該 session 的 bot 只看這個欄位。
- `projects[].host` 永遠存在（本機 `"local"`）；sidebar 徽章在 `host !== "local"` 時才顯示。
- host 斷線時其下所有 bot 的 `lamp` 一律 `"disconnected"`。
- 另有 `hosts[].tools`、`identities`、`shell_identities`、`attach_command`（§12、身份一節）。

### `POST /api/hosts`
新增或更新遠端主機，寫回 `[[hosts]]`，**同步等第一次連線結果**（ensure session + ssh master + ping，最長約 35 秒）才回應。

```json
{ "name": "m4p", "ssh": "m4p@100.112.229.82", "ssh_port": 22, "herdr_session": "agents-manager", "remote_path": "/opt/homebrew/bin:$HOME/.local/bin", "ssh_opts": ["-i", "/path/to/key"] }
```

| 欄位 | 必填 | 預設 |
|---|---|---|
| `name` | ✅ | `[a-z][a-z0-9_-]{0,31}`，`"local"` 保留 |
| `ssh` | ✅ | `user@host` 或 ssh_config 別名 |
| `ssh_port` | | `22` |
| `herdr_session` | | `"agents-manager"` |
| `remote_path` | | `""`（前置到遠端 PATH）。以 `:` 分項、每項各自 quote；項目開頭的 `$HOME`／`${HOME}`／`~` 展開成遠端 home，其他 `$`、`;` 都是字面（#241） |
| `ssh_opts` | | `[]`，原樣附加到每個 ssh 指令 |

回 `200 {"name","connected","error"}`；連不上仍 200（設定已寫入）。名稱不合法或為 `local` 400。同名視為更新（先斷舊連線）。

### `DELETE /api/hosts/{name}`
`200 {}`；仍有 project 使用 → `409 {"reason":"host still used by projects","project_id"}`；`local` 400。刪除時推 `host_changed {"connected":false,"error":"removed"}` 與 `project_changed`。

### `POST /api/hosts/{name}/reconnect`
強制重建 ssh master 與訂閱，回應同 `POST /api/hosts`。`local` 也可（重新 ping）。找不到 404。

### 遠端 project
`POST /api/projects` 帶 `host`（未設定的 host → 404）。遠端 `path` 不在本機 canonicalize，daemon 經 ssh 確認目錄存在並取遠端 canonical path，失敗 400。
目錄瀏覽 `GET /api/fs/dirs?host=` 只走 ssh 不經 herdr：host 斷線時仍可能 200，只有 ssh 失敗才 502；host 不存在 404。

### WebSocket
- `daemon_status`：`{"herdr_connected","default_connected","hosts":{"local":{"connected","error"},…}}`。
- `host_changed`：連上／斷線／新增／刪除／設定變更時推。狀態改變時該 host 底下每個 bot 也會收到 `bot_status`。

### GitHub CLI 登入

`GET /api/hosts/{name}/gh` → 該主機 `gh` 的登入狀態（issue 列表靠它）：

```json
{ "name": "m4p", "installed": true, "path": "/opt/homebrew/bin/gh", "logged_in": false, "account": "eddysun-alt",
  "accounts": [ {"login": "eddysun-alt", "active": true, "ok": false}, {"login": "Eden-Sun", "active": false, "ok": true} ],
  "mode": null, "pending": null, "error": null }
```

- `logged_in`：**作用中**帳號的 `gh auth status --json` 為 success 才 true。`pending` 是進行中的裝置碼（不含 `device_code`、token）。
- host 不存在 404；ssh 失敗 502；沒裝 gh → `installed:false`（仍 200）。

`POST /api/hosts/{name}/gh/login {"mode"?: "auto", "user"?: null}`：

| mode | 行為 |
|---|---|
| `auto` | 已可用 → 原樣回；有有效但非 active 的帳號 → `switch`；遠端且本機已登入 → `copy`；其餘 → `device` |
| `switch` | `gh auth switch --hostname github.com --user <user>`（沒給 user 切到第一個有效的非 active 帳號） |
| `copy` | 本機 `gh auth token` 經 ssh **stdin** 餵給遠端 `gh auth login --with-token --insecure-storage`（token 不進 argv、log）；只適用遠端 |
| `device` | daemon 向 GitHub 要裝置碼、立刻回 `pending {user_code, verification_uri, verification_uri_complete, expires_in}`；背景輪詢，授權後同樣 `--with-token` 餵給該主機 |

回應同 GET 外加實際 `mode`；前端每 ~2 秒 GET，`logged_in:true` 完成、`error` 有字失敗。token、`device_code` 絕不出現在 JSON 或 log。
錯誤：host 不存在 404；`mode` 不合法 400；`copy` 打在 `local` 400；`copy` 但本機未登入 `409 {"reason":"local_gh_not_logged_in"}`；沒有可切的帳號 400；上游失敗 502（message 已打碼）。

`POST /api/hosts/{name}/gh/cancel` → 放棄裝置碼，回應同 GET（`mode:"cancel"`、`pending:null`）。

## 主機 shell

在某台主機開**純 shell** 的 herdr pane（裝工具、看 log、清 worktree）。沒有 agent／run／turn，不發 WS 事件，由呼叫端輪詢終端快照。

- daemon 只認**它自己開的** pane：除建立外每支端點先在記憶體清單找 `(host, pane_id)`，找不到 → `404 {"what":"shell"}`。清單不落地，daemon 重啟後舊 pane 一律不認。
- pane 開在該主機 manager 的 session，不往使用者的 `default` session 開。

| 方法 | 路徑 | 說明 |
|---|---|---|
| POST | `/api/hosts/{name}/shells` | `{"cwd"?}`（省略取該主機任一 live project 的 path，否則 `$HOME`）→ `{host,pane_id,tab_id,workspace_id,cwd,herdr_session,created_at}` |
| GET | `/api/hosts/{name}/shells` | `{host,max:8,shells:[…]}`；回之前逐列 `pane.get`，pane 已不在就移除（herdr 問不到時保留） |
| GET | `/api/hosts/{name}/shells/{pane_id}/terminal?source=visible&lines=200` | `{host,pane_id,cwd,source,text,revision,truncated,columns,rows}`；`source`／`lines` 同 §7 |
| POST | `/api/hosts/{name}/shells/{pane_id}/text` | `{"text","enter"?:true}`；Enter 是另一次 `send_keys(["enter"])`；`{"text":"","enter":true}` = 只按 Enter |
| POST | `/api/hosts/{name}/shells/{pane_id}/keys` | `{"keys":["ctrl+c"]}` 原樣送 herdr；空陣列 400 |
| DELETE | `/api/hosts/{name}/shells/{pane_id}[?confirm=true]` | `pane.close`（分頁空了一起收）。記憶體清單裡的直接關；清單沒有就照 `panes` 表關，規則同 `POST /api/panes/{id}/close`：`confirm` 沒帶＝false，服務 pane 或讀不到事實時回 `409 {"reason":"service_pane","pane":{…含 kind／listen_ports／read_only},"unverified":bool}`，agent／active run 403；兩邊都沒有 404 |

- 建立：先借該主機某 project 的 workspace 開新 tab，借不到才 `workspace.create`（標籤 `shell`，不寫回 `projects.workspace_id`）。回的 `cwd` 是 herdr 實際開起來的目錄。
- **鍵盤同步**（UI 的「鍵盤同步」開關）沒有新端點：每一下按鍵是一次 `…/keys`（`herdrKeyFromEvent` 譯成 herdr 鍵名），貼上是一次 `…/text` 且 `enter:false`。
  兩者共用同一個前端佇列（`web/src/lib/keyQueue.ts`）：**同一時間只有一個請求在路上**，飛的期間按的鍵合成下一批。
  一鍵一個 POST 會同時在路上、抵達順序不保證——打 `ls` 可能變成 `sl`。貼上不拆成鍵：文字裡的換行會變成 Enter 直接執行。
- 每台最多 8 個：`409 {"reason":"too_many_shells","host","max":8}`。host 不存在 404；沒連線 502。
- **白名單兩份**（2026-09-16，SPEC §6.5e「選單點得進去」）：`terminal`／`text`／`keys` 除了這裡開的 shell，也接受 `panes` 表裡的 pane。
  **打字看 listen port，不看 `kind`**——本機有 port（pane 列 `read_only:true`）只可看，打字回
  `403 {"error":"read_only_pane","listen_ports":[…],"message":"…"}`；沒 port 的（含跑著 vim 的 `service`）可以打字。
  遠端不算 port，退回表上的事實：`kind=service` 或記過 port 就 403 `read_only_pane`。
  有 active run 的 pane 一律 `403 {"error":"agent_pane","message":"…"}`（連看都不給）。沒被 trace 的 pane 照舊 404。
  `text`／`keys` 之前即時問 herdr（結果重用 3 秒）：裡面現在有 agent 403 `agent_pane`、pane 不在 404、本機重對到 port 403 `read_only_pane`；
  **問不到就不放行**（herdr 沒回、`ps`／`lsof` 失敗或逾時、沒報 shell pid）：
  `409 {"error":"conflict","reason":"pane_state_unknown","retryable":true,"message":"無法確認這顆 pane 現在的狀態…，請稍後再試"}`。
  `DELETE` 同樣認兩份（daemon 重啟後面板自己開的 shell 只剩 `panes` 表認得；以前找不到就回 200、什麼都沒關），走 `panes` 表那條要照 `confirm`。
  一般要關被 trace 的 pane 仍走 `POST /api/panes/{id}/close`（服務 pane 會先 409 要人確認）。
- `recent` / `recent_unwrapped` 只給**已捲出畫面**的內容：沒捲過的 pane 兩者回 `text:""` + `truncated:true`，前端要說明而不是顯示空白。

## 身份 identities（SPEC §16）

同一種 agent 用不同帳號：identity = 一組具名 env + args，套在 bot 上。

```toml
[[identities]]
name = "cc1"                         # [a-z][a-z0-9_-]{0,31}
kind = "claude"                      # claude | codex | grok
# host = "m4p"                       # 選填；省略＝只適用本機。鍵是 (host, name)：
                                     # 同名的 cc1 在別台是別的帳號（SPEC §16.2）
args = []
[identities.env]
CLAUDE_CONFIG_DIR = "$HOME/.claude-ccompany"

  [[projects.bots]]
  name = "foo-cc1"
  kind = "claude"
  identity = "cc1"
  [projects.bots.env]                # bot 自己的 env（覆蓋 identity）
  FOO = "bar"
```

啟動 Run 時：pane env = daemon 注入 ∪ `identity.env` ∪ `bot.env`（後者覆蓋前者）；args = daemon 注入 ++ `identity.args` ++ `bot.args`。
env 值的 `$HOME`、`${HOME}` 與開頭 `~` 展開成**該 host 的 home**。identity 與 bot 的 kind 不符 → 400。

### `GET /api/state` 的身份欄位

```json
{
  "identities": [ {"name":"cc1","kind":"claude","host":null,"env":{"CLAUDE_CONFIG_DIR":"$HOME/.claude-ccompany"},"args":[]} ],
  "hosts": [ {
    "name": "m4p",
    "shell_identities": [ {"name": "cc0", "kind": "claude", "env": {}, "args": []} ],
    "identities": {
      "cc0": {"name":"cc0","kind":"claude","logged_in":true,"account":"me@example.com","plan":"max","source":"shell"},
      "cc1": {"name":"cc1","kind":"claude","logged_in":null,"reason":"…","source":"config","config_dir":"/Users/m4p/.claude-ccompany"}
    }
  } ],
  "projects": [ {"bots": [ {"id":"01M1…","identity":"cc1","env":{"FOO":"bar"}} ]} ]
}
```

- 頂層 `identities` 是 config.toml 那一份（沒設定 `[]`）；`hosts[].shell_identities` 是那台登入 shell alias 認出的 `cc0`…`cc6`（env 為字面值、`args` 永遠 `[]`）。
- `hosts[].identities.<name>`：`logged_in` 為 `true`/`false`/`null`（`null` = 未知，帶 `reason`）；`source` 為 `config`（可編輯）或 `shell`（唯讀）；`config_dir` 用那台 home 展開，預設帳號沒有。
- 同名時的優先序（`tools::merge_identities`）：明寫這一台的 config → 本機才有：沒寫 host 的 config → 那台的 shell `ccN` → 遠端才有：沒寫 host 的 config（名字是 `ccN` 時等那台偵測過才給）。identity 存在性在**該 bot／專案的 host 上**檢查，找不到 → `404 {"what":"identity"}`。
- 每個 bot 都有 `identity`（`string|null`）與 `env`（預設 `{}`）。
- **身分有 kind，bot 只能帶同 kind 的身分**（2026-09-14 使用者指正：`cc0`／`cc1`／`cc2` 是 Claude Code 的帳號代號，跟
  codex 無關）。API 建立／修改已經擋（kind 不符 400）；daemon 收編子 agent 的那條路也照這條：**只繼承同 kind 母 bot 的
  identity**，codex 子 agent 從 claude 母 bot 收編時 `identity = null`（之後 `pane_identity` 依它 pane 真正的帳號目錄只補
  同 kind 的身分）。子 agent 被重新收編成別的 kind 時，舊 identity 一併清掉。
- **偵測完一台主機的身分就清理一次**（`identity_kind::cleanup_host`）：kind 不符的 identity 設 `null` 並記 log（使用者自建
  的 bot 是 config.toml 設定，只記 warn 不改）；quota 表裡 `<kind>:<name>` 而 `name` 是別的 kind 的身分（例如 `codex:cc1`）
  那種殘留 key 刪掉，推一次 `quota_updated`（`quota: null`）。
- quota key 的組法見 §12.4：身分的 kind 跟 bot 不同 → 裸 kind；同 kind 才看該 kind 的 home 變數決定要不要分開；
  找不到那個身分（偵測還沒完成）維持分開。

### bot 建立 / 修改
`POST /api/projects/{id}/bots`、`PATCH /api/bots/{id}` 接受 `identity` 與 `env`：`identity` 省略 = 不變（PATCH）／`null`（POST），傳 `null` 或 `""` 解除；
`env` 傳整個物件會**取代**。不存在的 identity 404；kind 不符 400。

### `POST /api/identities` / `DELETE /api/identities/{name}?host=`
- POST `{name, kind, env, args, host?}` → `200 {"name"}`；名稱或 kind 不合法 400；未知的 host `409 {"reason":"unknown host","host"}`；
  **同一台**重複 `409 {"reason":"identity name already in use","name"}`——鍵是 `(host, name)`，同名在別台是另一筆（SPEC §16.2）。`host` 省略＝不寫 host：鍵算本機，本機優先、遠端讓位給那台同名的身分；只要本機傳 `"local"`。
- DELETE `?host=`（省略＝本機）→ `200 {}`；仍有**同一台**的 bot 綁著 `409 {"reason":"identity still used by bots","bot_id","host"}`。
  刪的是**沒寫 host** 的那筆時，遠端的 bot 也算：那台沒有自己同名的身分（config 明寫、或 shell 偵測到的 `ccN`；還沒偵測過照「沒有」算）就是在用這一筆，同樣 409。
- WS `identities_changed {}` → 重拉 state；bot 的 identity/env 變更沿用 `bot_changed`。

### `POST /api/hosts/{name}/identities/{identity}/login`
在該主機開**臨時 host-shell pane**，以該身份展開後的 env 執行 `claude /login` / `codex login` / `grok login`。env 只送進該 pane，不寫 daemon log 或事件。
回應是主機 shell 的 pane 物件；UI 從它的 terminal 顯示 device code / URL。登入指令結束後 daemon 重新探測該身份並關 pane。

| 狀況 | 回應 |
|---|---|
| identity 不存在 | `404 {"what":"identity"}` |
| 該 kind 的 CLI 不在偵測到的 PATH | `409 {"reason":"identity_login_unavailable","host","identity","kind","message"}`（`message` 是人話；`reason` 是機器 key，不能再用 `reason` 放中文否則會蓋掉） |
| host 不存在 / 未連線 / pane 建立失敗 | `404 {"what":"host"}` / 502 |

### `POST /api/hosts/{name}/identities/{identity}/logout`
同一條路、同一組 env，只是指令換成 `claude /logout` / `codex logout` / `grok logout`（回應與錯誤與 login 相同）。
env 前綴跟登入是同一段程式算出來的——少帶 `CLAUDE_CONFIG_DIR` 會登出**別的**帳號。
清掉的是該身份設定目錄裡的憑證：正在跑的 bot 不受影響，之後重新啟動會停在登入畫面。

## 10. Bot 欄位、編輯、重啟、刪除

### 10.1 bot 欄位

| 欄位 | 型別 | 說明 |
|---|---|---|
| `name` | string | 暱稱：1–32 字、允許 CJK，不可含空白或 `@ , : ;`；專案內唯一（重複 409 `bot name already in use in this project`）。執行中也可改，不影響 herdr |
| `agent_name` | string（唯讀） | herdr 內的 agent 名：有 active Run 時是實際啟動的名稱，否則是下次會用的 `<project slug>-<bot id 尾 6 碼>` |
| `kind` | `claude` \| `codex` \| `grok` | 其他值 400 `kind must be claude, codex or grok` |
| `model` | string \| null | `null` = CLI 自己決定；不做白名單驗證，空白字串正規化成 `null` |
| `effort` | string \| null | 依 kind 驗證，其他值 400。claude `low\|medium\|high\|xhigh\|max`；grok `low\|medium\|high\|xhigh`；codex `none\|minimal\|low\|medium\|high\|xhigh\|max\|ultra` |
| `fast` | bool | §12.2 |
| `auto_approve` | bool，預設 `true` | 注入略過權限確認的旗標：claude `--dangerously-skip-permissions`、codex `--yolo`、grok `--always-approve` |
| `inject_hooks` | bool，預設 `true` | `false` 時回覆走終端備援 |
| `primary` | bool，預設 `false` | 純顯示用釘選（標題列下面那一列排最前）。不影響 argv/env，永遠 `needs_restart:false`；不進 config.toml（`bots.is_primary`），child bot 也能釘，手機與電腦同步 |
| `identity` / `env` | 見身份一節 | |
| `persona` | §12.8 | |
| `instruction_files` | string \| null（唯讀給 codex／grok） | claude 才有：這顆 bot 讀哪份專案指示檔，§12.8b。`GET /api/state` 永遠給有效值（沒設＝`claude-md`），codex／grok 是 `null` |
| `managed_by` / `parent_bot_id` | 唯讀 | `user` = config.toml 的 bot；`child` = bot 自己開的子 agent（§子 agent） |

啟動 argv 順序（前端可據此預覽）：daemon 旗標（auto_approve、hooks、persona）→ model（claude `--model`、codex/grok `-m`）→ effort（claude `--effort`、grok `--reasoning-effort`、
codex `-c model_reasoning_effort="<level>"`）→ identity.args → bot.args。

### 10.2 `PATCH /api/bots/{id}`

body 所有欄位可省；`model`、`identity` 傳 `null` 或 `""` 清除；`env` 傳整個物件為**取代**：

```json
{ "name": "am-codex", "model": "gpt-5.5", "effort": "high", "fast": false, "args": ["--search"], "autostart": false, "auto_approve": true, "inject_hooks": true, "identity": "cc1", "env": {"FOO": "bar"}, "primary": false, "persona": "…", "instruction_files": "claude-md-and-agents-md" }
```

回 `200 {"needs_restart": bool}`，成功推 `bot_changed`。真的試過「當場套用」時多一個
`live_apply: {fields, applied, reason}`（2026-09-13）：`reason` 是機器可讀 key——
`bot_missing` / `no_active_run` / `slash_gate: <not_running|agent_busy|turn_in_flight|no_pane>` /
`no_herdr_client` / `<field>_cleared_to_default` / `no_slash_command_for_<field>` / `slash_send_failed` /
`not_a_single_field`，codex 另有 `codex: <picker_failed|unknown_fast_tier|fast_toggle_failed|no_status_line|
readback_model_mismatch|readback_effort_mismatch|readback_fast_mismatch>`。以前失敗是**靜默**的：
只回 `needs_restart: true`、log 也沒寫，「codex 改 effort 明明不用重啟，為什麼又重啟」查不出來。

- **`needs_restart: true`**：有 active Run 且動到影響啟動 argv/env 的欄位（`model`、`effort`、`fast`、`args`、`identity`、`env`、`auto_approve`、`inject_hooks`、`persona`、`instruction_files`）。
  沒有 active Run，或只改 `name` / `autostart` / `primary` → `false`。前端顯示「需要重新啟動」並提供 §10.3。
- **當場套用的例外**：只動了下列欄位、Run `running` 且不忙（非 working/blocked、無 in-flight turn）、新值不是清成 `null`（codex `fast` 例外）時，daemon 操作 TUI 並回 `false`；
  任一條件不成立或回讀對不上就回 `true`。細節見 SPEC §4.4a：
  - grok `effort` → `/effort <level>`；grok `model` → `/model <id> [effort]`。
  - claude `model` → `/model <alias>`；有對話紀錄時的「Switch model?」框 daemon 會按 `1` 確認，關不掉就 Esc 並回 `true`（不會把框留在畫面上）。
  - claude `effort` → `/effort <level>`（副作用：claude 存成該帳號新 session 的預設）。
  - codex `model` / `effort` / `fast` → 操作 `/model` 兩層選單與 `/fast` 開關（可一起改，只改 `fast` 也走這條），回讀狀態列確認，`run.runtime_*` 存讀回的值（副作用：寫進 `~/.codex/config.toml`）。
- `identity` 不存在 404；kind 不符 400。

### 10.2a `POST /api/bots/{id}/start` 對睡著的 bot（2026-09-17，SPEC §6.11）

這顆是 AGM 因為閒置收起來的（`asleep` 不是 `null`）時，start 走的是**續接**那條路
（`--resume <上一個 session>`），回應多一個 `resumed`：

```json
200 {"run_id":"01M1…","resumed":true,"session_id":"…","resume_outcome":null}
```

欄位語意同 `?resume=native`（§ 上表）：叫醒時接不回會退回開新對話，這時回 `resumed:false`，不寫死 `true`（issue #107）。
一般的 bot 行為不變（`200 {"run_id":"…"}`）。使用者按「啟動」想要的是把剛剛那顆帶著對話的 bot
叫回來，不是開一段新的空白對話。

### 10.3 `POST /api/bots/{id}/restart`
有 Run 先 stop（ctrl+c ×2、逾時關 pane）再 start → `200 {"run_id"}`（新 Run）。沒有 Run 也可呼叫（= start）。錯誤同 start，另加 stop 那一半的錯誤（同 `POST /stop`）。過程推 `bot_status`。
子 agent 在原 pane 重開（SPEC §6.9）。
`?resume=native`：同 start 的語意，**停之前**就判斷接不接得回（看現在這個 Run 的 session）；接不回回 `409 cannot_resume`，原本的 agent 不會被停。預設（不帶）行為不變。
重啟期間這顆 bot 排著的 queued（AGM 派工）**不撤**，留給新的 Run 送；重啟沒能把 bot 開回來才撤（SPEC §4.4a「重啟不是停」，issue #106）。
停掉了卻沒能開回來時，舊 Run 改標 `exited`；改標寫不進 DB 回 `503 {"error":"restart_state_uncommitted","run_id":<舊 Run>,"retryable":true,"message","detail","start_error"}`（`start_error` 是 start 那一半的錯），已排重試（SPEC §6.4）。

### 10.3c `GET /api/capabilities`
`200 {"capabilities":["resume_native_start","herdr_maintenance"]}`。會停 herdr server 的腳本先確認這裡有 `resume_native_start` 才動手。

### 10.3d herdr 維護狀態 `/api/supervisor/herdr-maintenance`（SPEC §6.5.2）
- `GET` → `{"active":bool,"window":{"opened_at","until","opened_by","reason"}|null,"max_minutes":30}`；過了 `until` 的窗口在讀取當下自動結束。
- `POST …/open` body `{"minutes"?:1..30（預設 30）,"reason":"必填"}` → `{"active":true,"window"}`。**只有 AGM 角色**（`X-AM-Bot-Id`＋該 bot 的 `X-AM-Bot-Token`）：其他呼叫端 `403 herdr_maintenance_forbidden`；已經開著 `409`；分鐘數越界或沒有理由 400。
- `POST …/end` body `{"reason"?}` → `{"active":false,"closed":true,"retired_children":["<name>",…]}`；沒開著回 `{"closed":false}`。同樣只有 AGM 角色。
- 開、關、逾時都寫 `supervisor_notes`（`herdr_maintenance_start`／`_end`／`_expired`）。

### 10.3b `POST /api/bots/{id}/fork`
從頂層 bot 分出一顆新 bot，讓它的 CLI 接續來源到目前為止的完整對話脈絡（SPEC §6.10）。body 可省略：`{"name"?:"alfa-fork"}`（省略＝`<來源>-fork`，撞名自動加 `-N`）。
- 設定照抄來源的 config.toml 條目（kind、model、effort、fast、persona、instruction_files、args、identity、env、auto_approve、inject_hooks），`autostart` 一律 false。建好立刻啟動。
- `200 {"bot_id","name","forked_from":{"bot_id","session_id"},"run_id"|null,"start_error"|null}`：建好但沒啟動成功仍回 200，`start_error` 帶原因。推 `bot_changed`；新 bot 的對話裡有一則系統訊息說明從哪裡分出來。
- 錯誤（都不會先建 bot）：來源不存在 404；`409 reason`：`fork_child`（子 agent）、`default_session`（從 herdr default session 匯入的）、`no_session`（還沒記到 native session）、`transcript_missing`（本機對話檔不在）、`unsupported_kind`、`not_in_config`；名字不合法 400。

### 10.3c `POST /api/bots/{id}/promote`
把子 agent（`managed_by="child"`）升級成頂層 bot（`managed_by="user"`、進 config.toml），**保留同一段 claude 對話**（SPEC §6.10a，issue #248）。body 可省略：`name`（省略＝沿用 child 的名字，它自己還占著就加 `-N`）、`model`、`effort`（省略＝沿用 child 的）。
- 只收 claude、本機、自己沒有子 agent 的 child。session 從 pane 裡活著的 claude 行程找（`<CLAUDE_CONFIG_DIR>/sessions/<pid>.json` 的 `sessionId`）；child 已停時用上一次做到一半記在它 run 上的 session。
- 步驟與收回：transcript **複製**（不搬、不覆寫）到新 bot cwd（專案路徑）對應的 `projects/` 目錄 → 停 child → 建 user bot → 種下 session → `resume=native` 啟動（接不回就不啟動）→ 收掉 child 紀錄。停不掉、建不成或啟動失敗都不留第二顆 bot：複製的檔與新 bot 收回。停掉 child 之後不可逆（它的 pane 是母 agent 開的），失敗回應帶 `child_stopped:true`，同一個請求可原樣再送。
- `200 {"bot_id","name","promoted_from":{"bot_id","session_id"},"transcript_path","run_id"}`。推 `bot_changed`（新舊兩顆）、`project_changed`。
- 錯誤：來源不存在 404；名字不合法 400；`409 reason`：`not_child`、`unsupported_kind`、`remote_not_supported`、`default_session`、`has_children`、`bot name already in use`（明給的名字撞名）、`session_not_found`、`session_ambiguous`、`transcript_exists`（目標已有不同內容的同名檔）、`transcript_copy_failed`、`stop_failed`、`promote_create_failed`、`promote_start_failed`（`rolled_back`、`child_stopped`）、`promote_child_not_removed`、`not_in_config`。

### 10.3a `POST /api/bots/restart-idle`
一鍵把「帶著 claude 更新且閒置」的 bot 全部 exit + resume（SPEC §6.9）。無 body。

```json
202 { "batch_id": "01M2…", "total": 2,
      "planned": [{"bot_id":"01M1…","name":"am-claude"}, {"bot_id":"01M1…","name":"C1-fable"}],
      "skipped": [{"bot_id":"01M1…","name":"am-claude-2","reason":"working","reason_label":"正在跑，重啟會把這一回合砍掉"}] }
```

- **202**：回的是計畫，重啟在背景一顆一顆跑。`total = 0` 也是 202，並立刻推 `bots_restart_done`。
- 已經有一批在跑：`202 {"batch_id": <那一批>, "total": 0, "planned": [], "skipped": [], "already_running": true}`，不另開一批、不推新的 `done`，進度照那一批的事件。
- 每顆 `restart_bot_with(resume_native)`，claude 拿到 `--resume <上一個 session>`（上下文不掉）；本機找不到 `transcript_path` 時開新對話。
- 候選 = claude 且 run 的 `update_notice` 非空；非候選不出現在任何清單。`reason`：`default_session` / `not_running` / `working` / `blocked` / `unknown_status` / `turn_in_flight`，
  以及輪到它時已經不是候選的 `no_longer_pending`（每一顆真的重啟前會照同一張表再判斷一次）、輪到它時 DB 讀不到它的狀態的 `state_unreadable`
  （這次沒動它，更新還在等；不是 `no_longer_pending`），
  `reason_label` 是給人看的那句（前端直接顯示）。
- 讀不到誰是總管（`supervisors`）：整批不開，回 502、沒動任何一顆，稍後再按。走哪一條重啟路（子 agent 原 pane 裡 exit + resume、其餘 stop + start）由一次讀得到的 bot 決定：
  讀不到 bot 時那顆記進 `failed`（`error` 說是讀不到分類），不會改走一般 stop + start。
- 一顆失敗不中斷整批。AGM 在 `planned` 永遠排最後，60 秒內沒回來自動再啟動一次（inbox `supervisor_restart_retry`）；最終啟動失敗推 inbox `bot_restart_failed`。

WS：每顆兩次 `bots_restart_progress`（`restarting`，然後 `ok` / `failed` 帶 `error`）；輪到它時改判跳過的只有一次 `status:"skipped"`（帶 `reason`、`reason_label`），每顆之後另推 `bot_changed`；收尾一次 `bots_restart_done`：

```json
{"batch_id":"01M2…","index":1,"total":2,"bot_id":"01M1…","name":"am-claude","status":"restarting"}
{"batch_id":"01M2…",
 "ok":[{"bot_id":"…","name":"am-claude","run_id":"01M3…"}],
 "failed":[{"bot_id":"…","name":"C1-fable","error":"…"}],
 "skipped":[{"bot_id":"…","name":"am-claude-2","reason":"working","reason_label":"…"}]}
```

`done` 的三張清單是權威（補齊漏掉的 progress）；用 `batch_id` 擋掉別的分頁那一批。

### 10.4 `DELETE /api/bots/{id}`
`200 {}`（有連帶刪子 agent 時 `{"removed_children":["<bot_id>",…]}`；有沒清掉的 runtime 目錄時另帶 `"kept_dirs":[{"bot_id","reason"}]`）。
`kept_dirs[].reason`：`stop_not_confirmed`（停機失敗，run 已強制收成 `exited` 但 agent 可能還活著）、`run_state_unreadable`（讀不到 active run）、
`run_still_active`（收完 run 仍是 active）——都表示 bot 已軟刪、只有目錄留著，下次開機的清掃在 run 確定結束後收（SPEC §3.1）。
定案之前讀不到 bot 或 child 的 active run → 502，什麼都不動，可原樣重試。

- 流程：有 active Run 先 stop（host 連不上送不出去時 run 直接標 `exited` 照常刪）→ 從 config.toml 移除 → `bots.deleted_at`（**對話與訊息保留**，`GET /api/bots/{id}/messages` 仍讀得到）→
  刪 `~/.config/agents-manager/bots/<bot_id>/`（遠端 ssh `rm -rf`，失敗只 log）。
- **子 agent 一起刪**：`managed_by = "child"` 且 `parent_bot_id` 指到它的（含孫代），最深的先。每顆各推 `bot_changed`。
- 找不到 404。
- daemon 啟動時掃一次 `bots/`，只刪 DB 裡已 `deleted_at` 且沒有 active Run 的 hook 材料目錄。

### 10.4a `POST /api/bots/{id}/restore`
軟刪復原：`200 {"bot_id"}`，推 `bot_changed` / `project_changed`。child 直接清 `deleted_at`；user bot 把 config.toml 那一筆加回去再投影。

| 狀況 | 回應 |
|---|---|
| 找不到 | `404 {"what":"bot"}` |
| 還沒刪 | `409 {"reason":"bot is not deleted","bot_id"}` |
| 同專案已有同名活著的 bot | `409 {"reason":"bot name already in use in this project","bot_id","name","taken_by"}` |
| 投影閘門擋下 | 同 §1 `projection_refused` |

### 10.5 `POST /relay/announce`
**不在 `/api` 下**，不吃 UI token：呼叫者是 pane 裡的 herdr shim，驗證用該 bot 的 hook token（`X-AM-Bot-Token`）。
表單編碼 `bot_id`、`to_agent`、`text` → `200 {}`；bot 不存在、已刪或 token 不符 → 401。
daemon 記在行程內（5 分鐘、認領一次就用掉），該句回音進對話時帶上 `relay_from`（SPEC §6.5d）。

### 10.6 hook 端點 `POST /hook/{claude|codex|grok}`
body `{bot_id, provider, payload, received_at, truncated?, run_id?}`，header `X-AM-Bot-Token`。`run_id` 是送出這則 hook 的 CLI 行程
啟動時的 `AM_RUN_ID`；只用來判世代（`--resume` 前後兩個行程回報同一個 session，SPEC「世代圍籬」），缺了就只看 session。
**`200` ＝ 事件已經寫進 `hook_events` 並 commit**（不是「已經處理完」，配對由 worker 背景做；SPEC §4.4b）；
回 `{"stored": true|false}`，`false` ＝同一則之前就收過了（重送，靠 `dedupe_key` 去重，不會變成第二筆）。
寫不進收件匣回 **503**，送端必須把同一份 body spool 起來稍後重送（SPEC §4.4 第 4 點）；`401` 壞 token、`410` bot 已刪除。
StatusLine 例外：不進收件匣，照舊 fire-and-forget 回 200。grok 的 `payload` 是 stdin JSON（`hookEventName`、`sessionId`、`promptId`、
`transcriptPath`、`lastAssistantMessage`、`reason`、`stopHookActive`）；`reason ≠ end_turn` 與 `session_end` 忽略，`session_start` 只回填 `native_session_id`（SPEC §12.3）。

## 11. 專案群組聊天（SPEC §13）

一個 Project 就是一個群組：`@<暱稱>` 或 `@all` 經 §5 的 prompt 路徑送給每個目標 bot，回覆回到合併時間軸。不自動啟動 bot。

- **`messages.group_id`**：同一次 chat 產生的 user 副本與「未送達」system 註記共用（= 該次 `client_request_id`），其他訊息 `null`。
- **mention（daemon 為準，前端一致）**：`@all` = 專案內所有 bot；`@<name>` token 為連續非空白、非標點字元（支援 `@小幫手，看一下`），不分大小寫；
  `@` 須在開頭或非字元之後（`me@example.com` 不算）。目標依專案內 bot 順序去重。送給 bot 的文字去掉 mention 與其後的 `, : ; ，：；、`。

### 11.1 `GET /api/projects/{id}/messages?before=<message_id>&limit=100`
Project 底下所有存活 bot 的訊息合併，以插入順序（`rowid`）倒序分頁、回傳正序；`limit` 1–500。每則多 `bot_id`、`bot_name`。Project 不存在 404。

```json
{ "project_id": "01M1...",
  "messages": [
    { "id": "01M1...", "role": "user", "content": "@all Reply with exactly GROUP-OK", "source": "web", "group_id": "c-group-1", "bot_id": "01M1...", "bot_name": "g-claude", "...": "…" },
    { "role": "assistant", "content": "GROUP-OK", "source": "hook", "group_id": null, "bot_name": "g-claude", "...": "…" }
  ],
  "has_more": false }
```

### 11.2 `POST /api/projects/{id}/chat`
`{ "text": "@all Reply with exactly GROUP-OK", "client_request_id": "…", "attachments"?: [...] }`。`client_request_id` 即 `group_id`；每個 bot 的 prompt 用 `<crid>:<bot_id>` 冪等，重送回同一組 `turn_id`、不重寫註記。此格式是群組未讀查詢的穩定候選契約；前端仍需以同回合 user 訊息的 `group_id` 核實群組來源。

```json
200 { "group_id": "c-group-4", "project_id": "01M1...",
      "sent": [ { "bot_id": "01M1...", "bot_name": "g-claude", "turn_id": "01M1...", "message_id": "01M1...", "delivery": "ok" } ],
      "skipped": [ { "bot_id": "01M1...", "bot_name": "g-codex", "reason": "not_running", "detail": "bot has no active run" } ] }
```

- 部分略過仍 200。`sent[].delivery` 同 §5。`skipped[].reason`：`not_running`、`blocked`、`in_flight`、`unknown_delivery`、`conflict`、`not_found`、`bad_request`、`upstream`；
  `detail` 即單 bot prompt 的 409 reason。每個略過的 bot 對話多一則同 `group_id` 的 system 訊息（經 `message_added` 推）。
  **字已經送進去、送達結果寫不進 DB**（單 bot prompt 的 `503 delivery_state_uncommitted`，§5，#149）不是略過：那個收件人放進 `sent`，
  `delivery` 依 `owed_delivery::owed_as_unknown` 標成 `unknown`（herdr 明確拒收才是 `failed`），帶 `turn_id`／`message_id`，**不**插「群組訊息未送達」。
  daemon 自己補上結果；同一個群組訊息（同 `client_request_id`）重送走冪等分支，不會再打字，DB 好了之後拿到寫好的 `delivery`。
- 錯誤：`400 {"error":"no_mention","message","bots":[{id,name,kind}]}`；`400` 空 text；`404` project。
- WS 沒有新事件：各 bot 各自推 `message_added`（帶 `group_id`）與 `turn_updated`，前端依 `bot_id → project_id` 歸群組。

### 11.3 第二個 daemon 實例
資料目錄＝`[server] data_dir` > `--config` 所在目錄 > `AM_DATA_DIR` > `~/.config/agents-manager`（`hook` 子命令讀 `AM_DATA_DIR`）。
驗證用：`AM_DATA_DIR=/tmp/am-x agents-managerd serve --config /tmp/am-x/config.toml`（另一個 `listen` port 與 `herdr_session`）。

- `--config` 指到非預設路徑時資料目錄**一定**跟著設定檔走，不會沿用預設目錄；`AM_DATA_DIR` 與它不一致 → 拒絕啟動。
- 同一個資料目錄同時只准一顆 daemon（`daemon.lock` 的 `flock`），拿不到鎖就拒絕啟動，連預設 config 都不會寫出來。
- 解析出來的資料目錄寫進 hook 的 argv（`--data-dir`）並注入本機 pane 的 `AM_DATA_DIR`，spool 跟著走；遠端則多一層 `instances/<slug>`。
- 隔離實例不認領既有 pane（要在它底下重啟那顆 bot）。
- `DELETE /api/bots/:id`、`DELETE /api/projects/:id`：目標此刻不在 config.toml → `409 {"error":"conflict","reason":"not_in_config"}`；除了這次要刪的 id 還有別的列會不見 → `409 … "reason":"delete_refused"`。兩者都什麼都不動（`DELETE /api/bots/:id` 不停 bot、不刪 child）。
- 只換 `listen` port **不算隔離**：2026-09-14 這樣起的第二顆開到正式 DB，被空 config 投影軟刪 15 顆 bot（SPEC §3.1）。

## WS `turn_progress`（即時輸出）

回合 `in_flight` 且 `delivery=ok` 時，daemon 每 0.7 秒讀 pane（`recent_unwrapped`），推 prompt 回音之後、清掉 TUI 雜訊的文字：

```json
{"type":"turn_progress","seq":123,"data":{"bot_id":"…","run_id":"…","turn_id":"…","text":"目前為止的部分回覆","activity":"Thinking… (12s · ↑ 1.2k tokens)","alert":"","revision":42}}
```

- `text`：目前為止的部分回覆（同最終訊息的清理規則）。
- `activity`：該回合畫面上最後一行 spinner／事件行（去開頭字元，≤ 120 字元，沒有送空字串）。純文字、不寫 DB、不進最終訊息，只在 `text` 還空時顯示——agent 純思考／跑工具時畫面只剩被清理掉的
  spinner 行，沒有它氣泡會永遠停在「等待回覆」。辨識取聯集、以最後命中的一行為準：glyph 白名單（claude/codex `✻ ✽ ✶ ✳ ✢ ·`、grok `◆`），或結構後備 `is_activity_shape`
  （`<單字>… (…)` 且括號內含 `tokens` 或 `12s`/`3m`/`1h`）。**spinner 動詞是隨機的**，不可字面比對。
- `alert`：重試／API 錯誤橫幅（`API error · Retrying in 0s · attempt 1/10`、`stream error: 503 upstream; retrying 2/5 in 1s`），≤ 120 字元、純文字、不寫 DB。
  CLI 重試上游時回合看起來完全健康，這是唯一說出這件事的訊號。辨識：最後一行 ≤ 240 字元且**同時**有 error／錯誤／overloaded 與重試 token（retry／retrying／attempt／reconnect／retries／重試），
  或以 `API error` 開頭（兩條件都要，擋掉 agent 回覆裡談論錯誤的散文）。
- **節流**：同一 `run_id` 每秒最多 4 幀（間隔 ≥ 250 ms），窗內只留最新一幀（快照非增量，不漏內容）；回合結束時壓著的最後一幀無條件補送。
- 只在 `text`／`activity`／`alert` 任一變化時推；回合結束後停止。前端顯示為即時氣泡，收到同 turn 的 assistant `message_added` 或非 in_flight 的 `turn_updated` 時移除。

## 12. 模型、fast、attach、額度、工具、人設、GitHub

### 12.1 `GET /api/models?kind=claude|codex|grok&host=<name>&identity=<name>&refresh=1`

某 host 上某 kind 可用的模型。`host` 省略 = `local`；快取 10 分鐘（key = host+kind+identity），`refresh=1` 強制重抓；遠端經 ssh 跑同一條管線。
`identity` 只對 claude 有意義：決定讀哪個 `CLAUDE_CONFIG_DIR/settings.json` 算 `default_effort`（SPEC §17.1）；省略或不存在 → 預設帳號（不是錯誤）。

```json
{ "kind": "codex", "host": "local", "source": "codex-app-server" | "grok-cli" | "static", "fetched_at": "2026-09-06T10:00:00.000Z",
  "models": [ { "id": "gpt-6-astra", "display_name": "gpt-6-astra", "description": "…", "is_default": true, "default_effort": "medium",
                "efforts": ["low", "medium", "high", "xhigh", "max", "ultra"],
                "service_tiers": [{"id": "priority", "name": "Fast", "description": "2x speed, increased usage"}] } ] }
```

| kind | source | 來源 | efforts | service_tiers |
|---|---|---|---|---|
| `codex` | `codex-app-server` | `codex app-server` JSON-RPC `model/list` | 每個模型的 `supportedReasoningEfforts` | 每個模型的 `serviceTiers`（目前只有 `priority` = Fast） |
| `grok` | `grok-cli` | `grok models` + `~/.grok/models_cache.json` 的 per-model `reasoning_efforts`（無 cache 退回 low/medium/high） | 依模型 | `[]` |
| `claude` | `static` | `opus / sonnet / haiku / fable` | 每個 alias 都是 `low…max` 五級 | `[]` |

- `default_effort`：codex/grok 是模型回報的值（可能 `null`）；claude 一律有值——帳號 `settings.json` 的 per-model 覆寫 > `effortLevel` > 內建 `"high"`。
- `display_name` / `description` 可能為空字串。
- 失敗（CLI 不存在、逾時、解析失敗、ssh 失敗）→ 502，**前端退回靜態清單**。`kind` 不合法 400；host 不存在 404。

### 12.2 `bot.fast`
布林，預設 `false`；TOML `fast = true`；POST 可省，PATCH 可改（有 active Run 時列入 `needs_restart`，codex 可當場套用）。

| kind | model | effort | fast |
|---|---|---|---|
| `codex` | `-m <model>` | `-c model_reasoning_effort="<effort>"` | 一律帶：`-c service_tier="priority"`（勾）或 `-c service_tier=""`（沒勾），見 SPEC §4.4a |
| `grok` | `-m <model>` | `--reasoning-effort <effort>`（模型不支援的等級啟動時丟掉）。TUI 不理這個參數也不理 config 的 `default_reasoning_effort`（#215）：就緒後若框底不是設定的等級，daemon 補送 `/effort <level>` | 不注入 |
| `claude` | `--model <model>` | `--effort <effort>` | 不注入 |

### 12.3 `hosts[].attach_command`
唯讀字串，貼到終端即可接上該 host 的 herdr session：`local` → `herdr --session <s>`；遠端 port 22 → `herdr --remote <ssh> --session <s>`；其他 port → `herdr --remote ssh://<ssh>:<port> --session <s>`。

### 12.4 額度 `GET /api/quota?refresh=1&host=<name>`

額度**按主機**分（SPEC §14）：本機裸 key，遠端加 `<host>/` 前綴。每台主機的三個基本 kind 一定在 map 裡，沒資料 `null`。

```json
{ "kinds": {
    "codex": {
      "five_hour": {"used_pct": 12.5, "resets_at": "2026-09-06T14:00:00.000Z", "low": false, "critical": false},
      "seven_day": {"used_pct": 40.0, "resets_at": "2026-09-12T08:00:00.000Z", "low": false, "critical": false},
      "reset_credits": {"available": 1, "title": "Full reset (Weekly + 5 hr)", "expires_at": "2026-10-11T05:31:28.000Z"},
      "limit_hit": null,
      "plan": "pro", "updated_at": "2026-09-06T10:00:00.000Z", "source": "codex-app-server", "host": "local" },
    "claude": {
      "five_hour": {"used_pct": 97.0, "resets_at": "…", "low": true, "critical": true},
      "seven_day": {"used_pct": 22.0, "resets_at": "…", "low": false, "critical": false},
      "fable": {"used_pct": 39.0, "resets_at": "…", "low": false, "critical": false},
      "reset_credits": null, "plan": null, "updated_at": "…", "source": "statusline", "account": null, "host": "local" },
    "claude:cc1": { "…": "同上，account = \"cc1\"" },
    "m4p/claude": { "…": "m4p 上讀到的同一組欄位，host = \"m4p\"" },
    "grok": { "five_hour": null, "seven_day": {"used_pct": 14.0, "resets_at": "…", "low": false, "critical": false}, "plan": "SuperGrok", "updated_at": "…", "source": "grok-usage", "host": "local" }
} }
```

- **key**：`codex`、`claude`、`grok`；有 identity 的 bot 另存 `<kind>:<identity>`（`account` = identity 名）；遠端加 `<host>/`。沒裝 CLI、還沒讀到 → `null`。
  不屬於現存主機的 `<host>/…` key 不出現。
  **身分的 kind 跟 bot 不同一律寫裸 kind**（codex 不會有 `codex:ccN`，`ccN` 是 claude 的身分）。
  同 kind 時，**身分對某個 kind 是不是預設帳號，看的是那個 kind 的 home 變數**（codex＝`CODEX_HOME`、claude＝`CLAUDE_CONFIG_DIR`、
  grok＝`GROK_HOME`）：身分的 env 沒定義那個變數，那個 CLI 就用它自己的預設目錄，讀數收斂到**裸 kind**。例如 cc1 只設
  `CLAUDE_CONFIG_DIR`——對 claude 寫 `claude:cc1`，對 codex 卻是裸 `codex`；cc2 帶自己的 `CODEX_HOME` 才寫 `codex:cc2`，
  而且不借裸 `codex` 的數字。查不到那個身分就當它有自己的帳號（寧可多一格，也不疊兩個帳號的數字）。
  寫入端（codex statusline、撞限橫幅、claude statusLine）與查詢端（`limit_hit_for_bot`／`next_reset_for_bot`、
  supervisor 額度判讀、mission 挑身分）都走同一支 `quota::quota_base_for_host`。（2026-09-14：cc0 改用 cc1 之後額度列
  又冒出 `codex:cc1`，因為舊規則只把「env 整個空的身分」當預設。）
  **身分是 run 起來時的那個**（issue #238，`quota::billing_identity`）：bot 在跑的時候改身分要重啟才生效，這段時間 pane 裡還是舊帳號，
  statusLine 讀數、撞限、成功回合清撞限、派送與排隊的閘門都記在／看 `runs.runtime_identity`；沒有 run、或 run 沒記（收編的 pane、升級前的舊列）
  才用 `bots.identity`。讀不到 run 時閘門照擋（不拿設定的身分猜）。
- `used_pct` 0–100；`resets_at` RFC3339 或 `null`；`five_hour` / `seven_day` 任一可為 `null`。
- `fable`：Claude Max 方案的 Fable 週額度（`Current week (Fable)`），形狀同 `seven_day`；沒有這個桶一律 `null`，**UI 不畫也不佔位**。
- `reset_credits`（只有 codex）：`account/rateLimits/read` 的 `rateLimitResetCredits`——`available` = 可用張數，`title`/`expires_at` 取第一張 available 的。daemon 只讀不用。
- `limit_hit`：CLI 印的上限橫幅 `{"message","until": "…"|null,"at","bucket": "five_hour"|"seven_day"|"fable"|null}`。速率視窗可以顯示 0% 已用但 credits 用完，這一格是唯一說「現在收不了工作」的地方，所以**黏著**：
  不帶這欄的輪詢沿用舊值，直到 `until` 過了、或 codex 下一回合真的答完（`quota::clear_limit_hit_for_bot`）。帶 `bucket`（`five_hour`／`seven_day`／`fable`，claude 橫幅與 grok 撞額度畫面才有；grok 的 `until` 是保底，見 SPEC §6 的撞限寫入）的撞限，
  遇到那一桶的新讀數會校正：窗是撞限之後才開的就清掉，否則 `until` 取與該窗 `resets_at` 較早者。寫進該 bot 身份的 key。UI 標「被擋」並壓灰量表。
- `low` / `critical`：daemon 算好的門檻（`quota.rs` 的 `LOW_REMAINING_PCT = 30`、`CRITICAL_REMAINING_PCT = 5`，以剩餘 % 判斷）。**前端只讀旗標，不寫死百分比。**
  `low` → 顯示剩餘數字；`critical` → 側欄 bot 列提示。
- `?refresh=1`：立刻重讀 `local` + 每台已連線遠端（各主機併發、同一台三個 kind 也併發；claude 探測最久 40 秒、grok 25 秒）；`&host=` 只重讀那台（不存在 404）。
  最多等 20 秒就回當下的快照；還沒跑完的探測留在背景（跑完照樣推 `quota_updated`），回應帶 header `X-AM-Quota-Refresh: pending`。
- 背景輪詢：codex 5 分、claude 60 秒、grok 30 秒，每輪各主機併發。
- `source`：
  - `codex-app-server`：每 5 分鐘 `account/rateLimits/read`（遠端 ssh）。
  - `codex-statusline`：同一輪讀每個 running codex pane 底下 `… · 5h 90% left · weekly 48% left`，寫同一把 key（畫面比 app-server 目前這個窗還舊就不採用，同一個窗只增不減，見 SPEC §14.2）；只有剩餘 %，`resets_at`／`reset_credits`／`limit_hit` 沿用前一份。
  - `statusline`：claude bot 對話中，daemon 注入的 `statusLine` 把 `rate_limits.five_hour/seven_day` POST 到 `/hook/claude`（`hook_event_name = "StatusLine"`，不建 Turn）。
  - `claude-usage`：背景 pane 探測 `claude auth status --json` + `claude -p "/usage"`（SPEC §14.2），純文字一行一個桶，依序對應 `five_hour` / `seven_day` / `fable`
    （`Current session` / `Current week (all models)` / `Current week (Fable)`，其他 model 週列忽略）；`plan` 取 `subscriptionType`。
    每個有獨立 `CLAUDE_CONFIG_DIR` 的身份各探一次（該主機清單，含 shell `ccN`）；60 秒內剛被 statusLine 更新**且**登入狀態已知的跳過；沒登入的 park 30 分鐘、其他失敗 5 分鐘。
  - `grok-usage`：`am-quota` session 的 TUI `/usage` 探測，只有週額度（`seven_day`），`plan` 取 `Weekly limit (SuperGrok)` 括號。

### 12.5 WS `quota_updated`

```json
{"seq":57,"type":"quota_updated","data":{"kind":"m4p/claude","host":"m4p","quota":{ "five_hour":{…},"seven_day":{…},"fable":null,"plan":null,"updated_at":"…","source":"statusline","account":null,"host":"m4p" }}}
```

`kind` 是完整 key；前端把 `data.quota` 直接寫進 `kinds[data.kind]`。

### 12.6 工具偵測 `hosts[].tools`

主機連線成功（本機為 daemon 啟動）時用登入 shell 偵測三種 CLI：

```json
{ "tools": {
    "claude": {"installed": true,  "path": "/opt/homebrew/bin/claude", "version": "2.1.0 (Claude Code)", "logged_in": true},
    "codex":  {"installed": true,  "path": "/opt/homebrew/bin/codex",  "version": "codex-cli 0.120.0",   "logged_in": true},
    "grok":   {"installed": false, "path": null, "version": null, "logged_in": null} },
  "tools_checked_at": "2026-09-06T10:00:00.000Z" }
```

- `installed`：`"$SHELL" -lic 'command -v <kind>'` 找得到；`path`/`version` 是 `command -v` 與 `--version` 第一行。
- `logged_in`：`true|false|null`；claude 看 `~/.claude/.credentials.json`（或 Keychain `Claude Code-credentials`），codex `~/.codex/auth.json`，grok `~/.grok/` 的 auth 檔。
- `hosts[].identities.<name>` 的 `logged_in`/`account`/`plan` 另一條路：codex、grok 走 ssh（`codex login status` / `grok models`，遠端回「未登入」照樣寫回快取）；**claude 不走 ssh**（讀不到 Keychain 會誤答 false，遠端讀到的 false 一律丟掉），
  搭 §12.4 的 `claude-usage` 探測拿，所以第一輪額度輪詢（≤ 60 秒）後才從 `null` 變真答案，變了推 `host_changed`。
- 尚未偵測時 `tools`、`tools_checked_at` 為 `null`。
- `POST /api/hosts/{name}/tools/refresh` → 立即重新偵測 `200 {"name","tools","tools_checked_at"}`（host 不存在 404、ssh 失敗 502），並推 `host_changed`。

### 12.7 透過現有 agent 安裝 `POST /api/hosts/{name}/tools/install`
`{ "kind": "grok", "via_bot_id": "01M1…" }`：daemon 組一則安裝 prompt（官方安裝方式：claude `curl -fsSL https://claude.ai/install.sh | bash`、codex `npm i -g @openai/codex`、
grok `curl -fsSL https://x.ai/cli/install.sh | bash`；接著確認 `--version`、執行登入並原樣印出登入 URL），走 §5 送給 `via_bot_id`。
回 `200 {"turn_id","message_id","delivery"}`。bot 不存在 404；不屬於該 host 400；bot 不可送 → §5 的 409；`kind` 不合法 400。登入時 pane 會 `blocked`，使用者在終端快照處理；完成後打 tools/refresh。

### 12.8 bot 人設 `bot.persona`
`string | null`：附加到 agent system prompt 尾端的文字，不動專案裡共用的 `CLAUDE.md` / `AGENTS.md`。TOML `persona = """…"""`；POST 可省、PATCH 可改（`null`/`""` 清除，有 active Run 列入 `needs_restart`）。

| kind | 注入 |
|---|---|
| `claude` | `--append-system-prompt "<persona>"` |
| `grok` | `--rules "<persona>"` |
| `codex` | `-c developer_instructions=<TOML basic string>`（daemon 逃逸換行與引號） |

位置在 daemon 旗標之後、model 之前。AGM 的人設另走 `/api/supervisor/persona`（總管一節）。

### 12.8b bot 的專案指示檔 `bot.instruction_files`（issue #213）
claude 2.1.277 起，內建 `agents-md` plugin 決定專案指示檔讀哪幾份（`instructionFiles`）。daemon 在每顆 claude bot 啟動時寫進它的 `--settings`（SPEC §4「注入設定」），**值由這顆 bot 決定，不是全域開關**：

| 值（就是 CLI 的選項，只收這四個） | claude 讀什麼 |
|---|---|
| `claude-md`（沒設時的值） | 只讀 `CLAUDE.md`。跟 2.1.277 以前一樣，不會因為專案沒有 `CLAUDE.md` 就改讀寫給 codex 的 `AGENTS.md` |
| `claude-md-or-agents-md` | 有 `CLAUDE.md` 讀它，沒有才讀 `AGENTS.md`（CLI 自己的預設） |
| `claude-md-and-agents-md` | 兩份都讀——要讓這顆 claude 和同專案的 codex 共用同一份 `AGENTS.md` 就選這個 |
| `managed-only` | 專案與使用者自己的指示檔都不讀，只留組織管理的 `CLAUDE.md` 與 memory |

- TOML `instruction_files = "claude-md-and-agents-md"`；POST 可省、PATCH 可改（`null`／`""` 清回 `claude-md`，不是 CLI 的預設）。有 active Run 時列入 `needs_restart`——設定檔只在啟動時讀。
- `GET /api/state` 每顆 claude bot 都帶**有效值**（`claude-md` 就是沒設），codex／grok 是 `null`；前端 Bot 設定面板的「專案指示檔」就是這一格，只在 claude 的 `managed_by: "user"` bot 顯示。
- **400**：值不在上表（CLI 遇到選項以外的值會當成它自己的預設＝改讀 `AGENTS.md`，等於沒釘，所以在 API 就擋）；`kind` 不是 claude 卻帶值（`null`／`""` 放行）；`managed_by: "child"` 的 bot（被認領的既有 pane，沒有 daemon 的 `--settings`，設了也讀不到）。
- 手改 config.toml 寫了看不懂的值、或寫在 codex／grok 上：投影時丟掉（DB 存 NULL），bot 照樣讀 `claude-md`，不會把拼錯的值交給 CLI。
- fork（§10.3b）與還原已刪的 bot 都會帶著這個值。

### 12.9 GitHub 專案偵測與 issues

專案載入／對帳／`POST /projects` 時偵測 git origin（遠端 ssh），解析 `git@github.com:owner/repo.git`、`https://github.com/owner/repo(.git)`、`ssh://git@github.com/owner/repo`：
`projects[].github = {"owner","repo","url"} | null`（非 GitHub、沒 remote、git 失敗為 `null`；快取到下次對帳）。`POST /api/projects/{id}/github/refresh` → `{"project_id","github"}` 並推 `project_changed`。

- `GET /api/projects/{id}/issues?state=open|closed|all&limit=30&q=<關鍵字>&refresh=1&repo=<submodule path>`：該主機 `gh issue list …`，快取 2 分鐘。

  ```json
  { "project_id": "01M1…", "repo": "Eden-Sun/powertech-hub", "repo_path": "", "source": "gh", "fetched_at": "…",
    "issues": [ {"number": 42, "title": "…", "state": "OPEN", "labels": ["bug"], "url": "…", "updated_at": "…", "author": "Eden-Sun", "body_excerpt": "前 300 字，換行壓成空白"} ] }
  ```

- `GET /api/projects/{id}/issues/{number}?repo=` → `{"project_id","repo","issue":{…,"body":"完整 markdown"}}`（不快取；找不到 issue 也是 502）。
- `GET /api/projects/{id}/submodules?refresh=1` → `{"project_id","submodules":[{"path":"vendor/foo","github":{…}|null}]}`（快取 2 分鐘）。
  `repo=<submodule path>` 相對專案根，省略 = 專案本身；不在清單或沒有 GitHub origin → 400。
- 錯誤：`project.github` 為 `null` → `400 project has no GitHub origin`；`gh` 不存在／未登入／失敗 → 502（遠端走 `POST /api/hosts/{name}/gh/login`）；project 不存在 404。

## 子 agent（bot 自己開的 pane，SPEC §6.5a–c）

- daemon 起的每個 agent 帶一段預設人設 `lifecycle::child_agent_rules`（接在 `bot.persona` 前面，三種 kind 同一份）；claude 另外拿到改寫過的 herdr skill。
- 對帳時 herdr 裡沒被 bot 認領的 agent 會建成子 bot：`managed_by = "child"`、`parent_bot_id`、kind 取 herdr 偵測（偵測不到沿用父的）、不注入 hook，並建 `adopted = 1` 的 run。
  認父線索血緣（同 tab）優先，其次名字前綴 `<父 agent_name>-<字尾>`。子 bot `name` 取字尾，否則 herdr agent 名。
- 身份從子 agent 行程的環境變數判定（SPEC §16.6）；`model` / `effort` 從 `pane.process_info` argv 反推（grok 可退回終端標題 `Grok 4.6 (xhigh)`），只補空值。
- PATH 上的 herdr shim 自動補命名前綴、把帳號與 hook 環境帶進子 pane（SPEC §6.5b）。
- `GET /api/state` 的 bot 物件：`parent_bot_id`（頂層 `null`）、`managed_by`。子 bot 不進 config.toml；pane 消失即 `deleted_at`（對話保留）。UI 側欄縮排掛在父 bot 底下。
- 子 bot 的對話來自終端擷取：pane 的 `working → idle` 就是回合邊界，寫成 `origin = external` / `completed_fallback` 的 Turn（SPEC §4.3）。

## 附件（任意檔案；2026-09-14 起不限圖片）

CLI agent 只吃文字，所以附件是先把檔案放到 bot 所在主機，再把路徑寫進 agent 讀到的文字。
**任何檔案都收**——mime 只決定 UI 畫縮圖還是檔案晶片，agent 拿到的一律是路徑。

### `POST /api/bots/{id}/attachments?name=<檔名>`
body 直接是檔案位元組（**不是** multipart），`Content-Type` 就是該檔的 MIME；沒帶時當
`application/octet-stream`。

```json
200 {"id":"01M1…","name":"screenshot.png","mime":"image/png","size":10158,"path":"/Users/me/proj/.agents-manager/attachments/01M1…-screenshot.png"}
```

- 檔案落在 `<project.path>/.agents-manager/attachments/`（agent cwd 之內，沙箱化的 CLI 才讀得到），該目錄自動寫一個 `*` 的 `.gitignore`。
- 遠端專案經 ssh（`hosts.rs::ssh_put`）寫到遠端同路徑，daemon 另存本機副本供縮圖。
- 不看 mime（2026-09-14 前只收 `image/*`）。空 body、超過 12 MB 或其他輸入錯誤 400；超過 12 MB + 4 KiB 由 body limit 回 413（無 JSON）；bot／project 不存在 404。

#### 落地是 staging-first（issue #88，2026-09-18）
`attachments.state`：`staging` → `ready`／`failed`。`save()` 先用 `state='staging'` 插入一筆**帶著意圖路徑**的
row（`local_path`／`agent_path`／`host` 都已經定案），再真的寫檔／ssh 送出；成功轉 `ready`，寫檔失敗轉 `failed`
並 best-effort 刪掉可能已經寫出去的一半。只有 `ready` 能被 `resolve`／`GET /api/attachments/{id}`／
`prompt` 的 `attachments` 綁定；daemon 啟動時 `attach::reconcile_orphans` 把還停在 `staging`（上一輪寫到一半就
死掉）或 `failed`（best-effort cleanup 可能沒清乾淨）的 row 全部當孤兒——刪本機檔、best-effort ssh 刪遠端檔、
刪 row，可重複執行。舊資料庫的既有列（都是舊流程「檔案寫完才 insert」留下來的）打開時一律回填 `ready`。

### `GET /api/attachments/{id}`
回原始位元組（原 MIME）。要 `X-AM-Token`，UI 用 fetch 轉 object URL，不能直接放 `<img src>`。只有 `state='ready'`
的附件讀得到，`staging`／`failed` 一律 404。

### prompt / 群組聊天帶附件
`POST /api/bots/{id}/prompt` 與 `POST /api/projects/{id}/chat` 可帶 `"attachments": ["01M1…"]`：
- id 以 **project** 為範圍（群組一次上傳、每個收件 bot 拿同一路徑）；跨專案或還沒 `ready` 一律 `400 unknown attachment`。
- agent 收到「文字 + 附加圖片／附加檔案（請讀取這些檔案來查看）：<絕對路徑>」（全是圖片才說「圖片」）；時間軸存使用者原本打的字，並把附件物件陣列記在該則 user message 的 `attachments`。
- 綁定（`attach::bind`，把 `messages.attachments_json` 與每筆 `attachments.message_id` 一起寫）是單一 SQLite
  transaction：任何一步失敗（含 `UPDATE` 沒打中任何 row）整批 rollback，不會有 message 指向沒綁成功的附件。

## run 的附加欄位（`GET /api/state` 的 `bots[].run` 與 `bot_status` 事件）

| 欄位 | 說明 |
|---|---|
| `agent_title` | herdr `agent.list` 的 `terminal_title_stripped`（agent 自己替工作取的名字）。herdr 沒有標題事件，daemon 每 4 秒輪詢，變了才寫並推。前後的 `-` 去掉；只是 CLI 名字（`Claude Code`、`codex`）當 `null`。run 結束不清，UI 只在 run 活著時讀 |
| `status_line` | 使用者自己的 claude `statusLine` 命令輸出（ANSI 已去）。daemon 的 `agents-managerd statusline` 代跑它並把同一份輸出放進 POST `/hook/claude` 的 payload。只有 claude；沒設 `statusLine.command` 為 `null`；變了才寫 |
| `status_json` | statusLine 壓縮前的原始 JSON（`transcript_path` 以外整份），daemon 補 `account_email`（讀該身份設定目錄 `.claude.json` 的 `oauthAccount.emailAddress`）。變了才寫 |
| `herdr_session` | bot 與 run 都有；一般為 `null`（沿用 host 設定），從本機 `default` session 採用的是 `"default"`（SPEC §6.5.1） |
| `update_notice` | 等重啟套用的 claude 更新，固定字串 `"Update installed · Restart to update"` 或 `null`（SPEC §3.1）。單顆套用 `POST /bots/{id}/restart`，全部 `POST /bots/restart-idle` |
| `runtime_model` / `runtime_effort` / `runtime_fast` | run **實際**在跑的值（SPEC §4.4a），跟 `bot.*`（下次啟動的設定）分開。三個都 `null` = 不知道（收編的 pane），前端不比對不標 |
| `runtime_identity` | run 用哪個身分起來的（issue #238）：`""`＝沒有身分（預設帳號）、`null`＝不知道（收編的 pane、升級前的舊列）。改了 `bot.identity` 之後、重啟之前兩者不同；額度一律記在這個身分上 |
| `turn_error` | 上一回合被 API 中斷或額度拒絕時 pane 上那行原文，否則 `null`；下一回合開始清回（SPEC §4.3a）。命中時對話多一則釘在回合上的 system 訊息（`incomplete = 1`、附 `terminal_snapshot`），回合還 in_flight 就收成 failed。重送就是再 `POST /prompt` 最後一則 user 訊息 |

`turn_error` 為額度用盡（`You've reached your Fable limit…`）時，daemon 同時把該 bot 帳號的額度格標 `limit_hit`，`until` 取橫幅講的桶（`Fable` → `fable`，否則 5h）的 `resets_at`；
**不**在下一回合成功時清掉（換 opus 能跑不代表 Fable 恢復），只靠 `until` 到期。UI chip「⛔ Fable 額度用盡」、給重置時間與「改用 opus」，重送鍵在重置前灰掉。

## 快速 git（chat 標題列的 chip）

都在專案的 host、專案目錄裡跑（遠端 ssh），不開 worktree、不切分支。

- `GET /api/projects/{id}/git` → `{"git":true,"branch":"main","upstream":"origin/main","ahead":0,"behind":0,"changed":7,"untracked":3,"insertions":246,"deletions":9}`。
  `git:false`（其他欄位省略）= 不是 git repo；`changed` 來自 `status --porcelain=v2`，行數來自 `diff --shortstat HEAD`（未追蹤檔不算）；detached HEAD 時 `branch` 為 `null`。
- `POST /api/projects/{id}/git/commit {"message"}`：`git add -A && git commit -m`。`200 {"ok":true,"output"}`；空訊息 400；沒有變更 `409 nothing_to_commit`；失敗 `409 {"reason":"git_commit_failed","output"}`。
- `POST …/git/push`：有 upstream `git push`，否則 `git push -u origin HEAD`。`POST …/git/pull`：`git pull --rebase --no-autostash`。回應同 commit（`git_push_failed` / `git_pull_failed`），逾時 180 秒。

## 更新的 changelog `GET /api/changelog?kind=&host=&from=&to=`

更新徽章／批次重啟 chip 按下去先呼叫這支，把 changelog 放進確認框。

- `kind` 省略 = `claude`，支援 `claude`、`codex`（其他含 grok、未知字串一律 `found:false`）。`host` 省略 = `local`；不認得的 host 也是 200 `found:false` + `error`，不是 404。
- 沒給 `to` 時 daemon 在該主機再跑 `claude --version` 當 `installed_version`（磁碟已是新版，不快取）。`from` 有給就回 `from`（不含）到目標（含）之間每一版，新的在前。
- **codex 一定要給 `to`**：它的更新是 TUI 當場問（`✨ Update available! 0.153.4 -> 0.154.0`），新版還沒進磁碟；UI 從畫面那句解出 `from` / `to`。
- 來源：claude `https://raw.githubusercontent.com/anthropics/claude-code/main/CHANGELOG.md`；codex `https://api.github.com/repos/openai/codex/releases`（濾掉 draft／prerelease，`rust-vX.Y.Z` + body 併成同格式）。各自快取 10 分鐘，認 `## x.y.z`。
- **永遠 200**；抓不到就 `found:false` + `error`，UI 必須寫「找不到 changelog」。

```json
{ "kind": "claude", "host": "local", "installed_version": "2.1.5", "from_version": "2.1.3", "found": true,
  "sections": [{ "version": "2.1.5", "body": "- …" }, { "version": "2.1.4", "body": "- …" }],
  "source_url": "https://raw.githubusercontent.com/anthropics/claude-code/main/CHANGELOG.md", "error": null }
```

## 上游新版分診 `/api/release-triage/*`（SPEC §18.2c，issue #204）

CLI：`agents-managerd release-triage-check --kind <claude|codex> [--since <ver>] --json`（抓 feed、切條、分桶、寫帳本；抓不到 exit 1）輸出
`{"kind","from","to","pending":[{"version","kept":[{"id","text","categories":[]}],"unmatched":[{"id","text"}],"dropped_count"}]}`，`pending` 舊版在前。

- `GET /api/release-triage?kind=&version=` → `{publish_enabled, repo, rows:[{kind,version,status,entries:[{id,text,bucket,categories,rules}],verdicts,issues:[{marker,entry_ids,number,url,created_at,comment}],dispatched_at,attempts,publish_error,created_at,updated_at}]}`，新版在前；`status`：`pending|dispatched|judged|published|empty|failed`。
- `POST /api/release-triage/dispatched {kind, versions[]}` → `{kind, dispatched}`。`pending` → `dispatched`（CAS，同一版不被下一輪再派）。
- `POST /api/release-triage/verdicts {kind, version, verdicts:[{entry_id, verdict:guard|adopt|upgrade-arg|none, reason, module}], issues:[{entry_ids[], title, goal, suggestion, acceptance, verdict?, duplicate_of?}]}`。
  只收 `pending|dispatched|failed` 的版本（其他 409 `not_awaiting_verdict`）；**整份驗過才收**，不合格 400 `{error:"invalid_verdicts", problems:[…]}` 一次列完。
  沒有任何 issue 提案 → 版本 `empty`；否則 `judged`，`[release_triage] publish = true` 時當場 publish。回 `{kind,version,status,verdicts,issues_proposed,publish_enabled,publish}`。
- `POST /api/release-triage/publish {kind?, version?}` → `{publish_enabled, results:[{kind,version,result:{outcome:disabled|published|deferred|failed,…}}]}`。重試所有（或指定的）`judged` 版本；`publish = false` 時每一筆都是 `disabled`、gh 不會被呼叫。
- 設定：`[release_triage] publish = false`（預設）／`gh_bin`／`repo`。
- CLI：`bin/agm release-triage submit --file verdicts.json`；另有 `show`／`dispatched --kind K --version V…`／`publish`。

## 總管 AGM（SPEC §18）

全部走 `X-AM-Token`。未部署的 daemon 對這些路徑回 404（前端用 404 判斷「這台不支援」）。

### 狀態、啟停、模型切換
- `GET /api/supervisor` → `{configured,bot_id,project_id,model:"fable"|"opus",model_arg,identity:"cc0",effort:"low",status,status_detail,generation,cwd,quota_reset_at,remote:{…},pending_count,assignments:[]}`。
  `status`：`not_configured` | `stopped` | `starting` | `idle` | `busy` | `waiting_quota` | `failed`。`pending_count` = 未結案 assignment + 未 ack inbox。`remote` 同 `GET /api/supervisor/remote`。
  另有 `last_deploy:{sha,at}`＝現在跑的這顆 binary 的 commit（建置時編進去，拿不到 git 是 `unknown`）與**這顆 binary 第一次上線**的時間（記在 `<data_dir>/last-deploy.json`；binary 沒換的重啟不會把它往前推）；
  前端用它排除上次上線以前的舊重建申請。
  頂層欄位都是**巡檢**（SPEC §18.15）；另有 `role:"patrol"`、`remote_provider:"patrol"`、`stats:{wakes,events_delivered,duplicates,merged,last_wake_at,last_wake_reason,last_notify_at,notify_next_at}`、
  `responder`（同 `GET /api/supervisor/responder`）。

### 協調者（responder，SPEC §18.15）
- `GET /api/supervisor/responder` → `{configured,bot_present,bot_id,project_id,identity,model,effort,remote_control:false,status,status_detail,quota_reset_at,desired_running,watchdog:{attempts,next_at,gave_up_at},inbox_open,wake_pending,stats:{…}}`。`wake_pending` = 還沒送出、會叫醒它的事件數。
  `POST /api/supervisor/responder/start` 先記 `desired_running=true` 再啟動：啟動失敗回錯誤，但看門狗會照 §18.9 的退避重試。
  `status`：`not_configured` | `stopped` | `starting` | `idle` | `busy` | `waiting_quota` | `missing`（登記過但那顆 bot 被刪了；事件仍留在它的佇列，另推一則 `responder_bot_missing` 給巡檢）。
  `configured` 是「登記過」，`bot_present` 才是「那顆 bot 還在」：路由只看前者。`model`／`effort` 是設定值，
  `runtime{model,effort,started_at}` 是它現在實際跑的（`/model` 換過就會不一樣）。
  `setup` 會驗 `model`（`[a-z0-9][a-z0-9._-]{0,39}`）與 `effort`（`config::normalize_effort`），不合格 400——這兩個值會直接變成 CLI 的 argv。
- `POST /api/supervisor/responder/setup {identity?,model?,effort?}` → 同上加 `deployed`。冪等、只建立不啟動；預設沿用已存的（第一次 `cc0/opus/high`）。掛在**巡檢的專案**底下，cwd 是自己的 `supervisor/AGM-responder`（記在 `bots.cwd`）
  （`CLAUDE.md`、`persona.md`、`runtime.json{role:"responder",self_bot_id,responder_bot_id,manager_bot_id}`、`bin/agm`、`handoff.md`），args 空（rc off）。身分不存在 409 `identity_missing`。
- `POST /api/supervisor/responder/start {}` / `stop {}` → 同 GET。`start` 標應該在跑（看門狗會拉起），`stop` 先標不要再停。
- `GET /api/supervisor/responder/persona`、`PUT {text,expected_version?}` → 同巡檢的 persona 形狀加 `role:"responder"`；內嵌種子 `docs/goals/agm-responder-persona.md`。
- `POST /api/supervisor/setup {}` → 同上再加 `deployed:{cwd,agm_cli}`。冪等：建立專用 Project／Bot／cwd，寫 `CLAUDE.md`、`persona.md`、`runtime.json`（**不含 token**）、`bin/agm`、
  `handoff.md`（已存在不覆蓋）。args `["--remote-control","AGM"]`、`autostart=false`，**只建立不啟動**。`cc0` 不存在 409 `identity_missing`；專案裡有別的 `AGM` 409 `name_taken`——supervisor 列還沒記過 bot 時（第一次設定、或上一次寫進 config 之後部署／記錄失敗），daemon 目錄那個專案裡的同名 `AGM` 就是上一次寫進去的那一顆，接著用它、不回 `name_taken`（協調者的 `AGM-responder` 同一條，#181）。
  總管認持久化的 `bot_id`，改名不會多開一個。
- `POST /api/supervisor/start {}` / `stop {}` → 同 GET。`start` 後 `remote.status` 是 `requested`。`start` 標「應該在跑」、`stop` 標「使用者要它停」：不是經 `stop` 停掉的
  （被殺、崩潰、更新重啟沒回來），watchdog 自動再 `start`（30 秒，之後 60／120／300 秒退避，連續 5 次失敗寫進 `status_detail` 並停止）。`waiting_quota` 期間與 setup 後沒 start 過的不拉起。
  兩支都**先寫意圖再做副作用**，寫不進去就整個失敗、一步都不做，回 `502 could not persist desired_running (…)`，不會靜靜吞掉（issue #84，協調者的 start／stop 同規則）：
  這個旗標是 watchdog 唯一的憑據，回 200 卻沒寫進去的話，使用者停掉的 AGM 下一個 tick 就自己活回來。**啟動本身**失敗不撤銷意圖（回錯誤，交給 watchdog 重試）；
  還沒 setup 就 start 回 409 `not_configured`，不留下沒有 bot 可以對應的意圖。
- `POST /api/supervisor/fallback {}` → 同 status 加 `switched:bool`。在 cc0 的 `fable`／`opus` 之間切換，冷卻窗 30 分內最多自動切一次。只在總管 idle 且無 in-flight turn（或沒在跑）時切，
  回合中 409 `busy`。`/model` 走 `send_slash_line`（答「Switch model?」框）；live 套用不成而仍 idle 時改重啟套用。自動判斷（controller 每 10 秒，`supervisor/policy.rs`）：
  1. 5h 或 7d 任一 `critical` → `waiting_quota`，`quota_reset_at` = 最近的 `resets_at`；窗恢復即解除。
  2. 在 `fable` 且 Fable 週桶剩 < 5% → 切 `opus`，`quota_reset_at` = Fable 桶 `resets_at`。
  3. 在 `opus`、Fable 剩 ≥ 20%（或 `resets_at` 已過）、冷卻已過 → 切回 `fable`。
  讀不到額度就不動（未知不等於滿）。

### 健康與 incident
- `GET /api/supervisor/health` → `status`（`healthy`／`degraded`／`critical`）、AGM 狀態、bot running/busy/stopped 計數、host 連線、quota、`pending_assignments`（未結案，含 `awaiting_review`／`blocked`）、（另有 `release_triage`：`publish = true` 時帶 `gh_auth_ok`／`gh_auth_error`，否則 `null`）
  `awaiting_review`、`inbox_open`（三者分開不相加）；`manager_health{status,supervisor_status,daemon_connected}` 與 `system_health{status,open_incidents,incidents,blind_probes}`（`blind_probes` 非空＝那幾類探針上一輪查詢失敗，`status` 至少是 `unknown`），頂層 `status` 取兩者較嚴重者。
  daemon 每 30 秒檢查，指紋變化才推 WS `supervisor_health`；inbox `health_changed` 只在巡檢或協調者的嚴重度（`manager_health.status`／`responder_health.status`）或總管狀態（idle/busy 視為 running）真的改變時入列，總管 stopped/starting 期間不入列、恢復後補一則。
  `responder_health{status,responder_status,inbox_open,wake_pending,retry_at}` 單獨一格，**也併進**頂層 `status`（取較嚴重者）。
  協調者 `waiting_quota`、`desired_running` 卻沒在跑、或沒在跑（stopped／missing）而 `wake_pending>0` → `degraded`。
  `due_actions{pending,overdue,failing,by_kind,soonest,items,items_truncated}`＝「daemon 接下來要做什麼、什麼一直做不成」（issue #75、#97）：
  把六處**本來就存在 DB 裡**的到期時間讀出來擺在一起（排隊 prompt 的重試、交辦重送、等額度、協調者補送、總管看門狗、hook 事件），
  只讀不寫、不是新的排程器。`overdue`＝到期了還在名單上（掃描還沒輪到，或一直失敗）；`failing`＝`attempts >= 3`；
  `pending`／`overdue`／`failing`／`by_kind` 都是 **SQL 聚合算的精確數字**，不吃任何列表上限（#97：以前是從「每類最多 50 筆」的清單數出來的，積壓一多就少報，500 件卡住的 hook 事件長得跟 50 件一樣）。
  `by_kind` 每一類是 `{pending,overdue,failing}`。`soonest` 只看**有排定時間**的那些，所以一堆「等事件、沒有時間」的列不會把它擠掉。
  `items` 是 failing 的**樣本**（最多 20 筆、試最多次的在前、帶 `last_error`），`items_truncated:true` 代表還有更多沒列出來——要全部就看 `failing`。
  因為數字精確、樣本有界，這一段**不分頁**：它是健康快照裡一段固定大小的摘要，不是列表 API。讀不到時整段是 `null`——觀測不該變成新的故障點。
- `POST /api/supervisor/ops-alerts {source,reason,detail?}` → `{queued,inbox_event_id,event_key}`。排程腳本（`scripts/ops/*`）卡住、自己解不開時喊人：
  寫一則 inbox `ops_alert`（路由給巡檢、叫醒）。`source`／`reason` 各 1–64 字的 `[A-Za-z0-9._-]`（不合格 400），`detail` 截到 2000 字。
  event_key 帶小時格：同 `source`+`reason` 每小時最多一則（`queued:false` = 這小時已經有了）。
- `GET /api/supervisor/incidents?all=0|1` → `{incidents:[{id,kind,resource,severity,status,detail,occurrences,first_seen_at,last_seen_at,resolved_at}],open,all}`。
  `kind`：`host_disconnected` | `bot_stopped` | `assignment_stalled` | `assignment_undelivered` | `notify_exhausted`（SPEC §18.9）。一個 resource 同時只有一筆 open；開啟與恢復各推 inbox `incident_opened` / `incident_resolved`。
  `notify_exhausted` 例外：協調者建立時推（路由給協調者，開啟叫醒、恢復只記錄）；沒有協調者不入 inbox，只在這支與 `system_health` 看得到。

### 遠端入口
`GET /api/supervisor/remote` → `{status,stored_status,revoked,source,observed_at,observed_by,session_id,current_session_id,url,url_is_evidence,capability,ttl_secs}`。
`status` 只有 `requested` | `verified` | `unavailable` | `unknown`（SPEC §18.12）；`capability.status` 目前 `unsupported`。觀測超過 `ttl_secs`（900）或 AGM 換 session 退回 `unknown`（`revoked` 說明），`url` 不回（`url_is_evidence:false`）。
`POST /api/supervisor/remote {status,source,actor?,evidence?,url?}`：`source` 只收 `manual`（`provider` 保留、`argv` 拒絕）；`verified`／`unavailable` 需要 actor、非空 evidence 與當前 AGM run，15 分鐘後失效。

### 人設
- `GET /api/supervisor/persona` → `{stored:{version,hash,source,updated_at,seeded_from,length,text},embedded:{hash,length},loaded:{status,run_started_at,evidence},upgrade_available,needs_restart}`。
  持久版是權威（SPEC §18.11）。`loaded.status`：`unknown`（沒在跑）、`stale`（session 比人設舊，確定沒載到）、`unverified`（帶著這份啟動，但看不到 session 現在握著什麼）；`needs_restart` 只在 `stale` 時 true。
- `PUT /api/supervisor/persona {text,expected_version?}` → 同上。正文改變才版本 +1，並改寫 config.toml 的 bot persona 與 `persona.md`（副本，不要手改）。
  `expected_version` 對不上且正文不同 409 `version_mismatch`；副本同步失敗 409 `persona_sync_incomplete`（帶 `stored:true` 與 `version`，重送相同正文可修復且不加版本）。
- `POST /api/supervisor/persona/adopt-embedded {actor?,reason?}` → `{changed,version,hash}`：內嵌版取代持久版的唯一路徑。
- `GET /api/supervisor/build-inputs` → `{paths,embedded:[{path,symbol}],note}`：會編進 binary 的路徑（含 `docs/goals/agm-supervisor-persona.md`、`docs/goals/agm-responder-persona.md`、`scripts/agm.py`）。

### 核准與租約（SPEC §18.10）
- `GET /api/supervisor/approvals?id=<id>`：只回那一筆（清單本身只有最新 100 筆，排程腳本要確認的舊核准會被擠出去）。
- `GET /api/supervisor/approvals` → `{approvals:[{id,requester,purpose,scope,target_commit,status,decided_by,decided_at,reason,expires_at,client_request_id,wait_since,decisions:[{from,to,actor,reason,at}],…}]}`。
  `status`：`pending` | `approved` | `denied` | `revoked` | `consumed` | `superseded`。`consumed`／`superseded` 不覆寫 `decided_at`／`decided_by`（誰、何時核准的留著；誰用掉的在 `decisions`）。
  `decisions` 是 append-only 的決定歷程（那一列只留最後一個狀態）。
- `POST /api/supervisor/approvals {requester,purpose:"rebuild"|"restart",scope,target_commit?,expires_in_secs?,request_id?,supersedes?}` → 一筆 `pending`（回應多 `created`、`superseded`），並推 inbox `approval_requested` 給 AGM。
  `supersedes=<舊的 approval id>`：**同一個 requester、同一個 purpose** 換 commit 重新申請。舊的還是 `pending`／`approved` 而且沒過期時，同一個 transaction 裡標 `superseded`、
  它還沒送出的 `approval_requested` 一起收掉（`acked_by:"daemon"`），新的 `wait_since` 接過舊的等待起點（舊的已核准＝它的 `min(wait_since, decided_at)`）；
  舊的已經不能用就不動它、新的從頭算。requester 或 purpose 不同 → `409 approval_supersede_refused`，什麼都不寫。
  帶 `request_id`（穩定 id）時**冪等**：同一個 supervisor 下同一個 id 再送回**原本那一筆**（200、`created:false`），不新增、不重推 inbox；已經被 decide 的也照樣回它本人（狀態就是當時的裁示）。
  同一個 id 但 `requester`／`purpose`／`scope`／`target_commit` 不同 → `409 {"reason":"approval_request_mismatch", field, existing, requested, approval_id}`，原本那筆一個字都不動（另一顆 bot 撞同一個自然 id 拿不回別人的核准）。
  `expires_in_secs` 不參與比對：原本那筆的到期時間不會被重送改掉。不帶 `request_id` 就是舊行為，每次開一筆新的。
  CLI：`agm approval request --request-id <id>`（不確定送出去沒有時用同一個 id 重送，不要換新的）；`--supersedes <舊 id>` 帶 `supersedes`。
- `POST /api/supervisor/approvals/{id}/decide {decision:"approve"|"deny"|"revoke",actor?,reason?,expires_in_secs?}`：同 decision 重送回 `idempotent:true`。
  **第一個裁示定案**：`approve`／`deny` 只從 `pending` 條件寫入；`revoke` 從 `approved` 或 `pending`。寫不進去 → 409
  `{reason:"already_decided"|"decided_concurrently",status,decided_by,allowed_from}`，什麼都沒寫（後到的 deny 不會把 approved 改掉）。
  成功回 `decided_from` 與 `audit_note_id`；決定歷程 append-only 存在 `supervisor_notes`。帶角色 bot token 時 `decided_by` 記 `AGM:patrol`／`AGM:responder`。
  核准決定與租約續租共用 supervisor lock；核准紀錄遺失 409 `approval_missing`（不延長租約）。
- `GET /api/supervisor/maintenance/safety?exclude=<id,id>&approval=<id>&owner=<name>` → `{safe,working,in_flight,unreadable,blocked_waiting_for_user,queued_assignments,checked_at,excluded_bot_ids,
  escalated,waited_secs,escalation_approval_id,escalate_after_secs,delivering,held_leases,owner}`。唯讀快照；`blocked` 只回報不阻擋。
  CLI `agm lease safety --exclude-bot <id>` 傳此查詢。
  `escalated=true`＝最早那筆已核准未消耗的 rebuild／restart 核准等超過 `escalate_after_secs`（預設 1800，`AM_MAINTENANCE_ESCALATE_MINS` 可調；從 `min(wait_since, decided_at)` 算），
  此時 `safe` 只看 `delivering`（送達臨界區：`queued` 或 `in_flight`＋`delivery='pending'`）、`held_leases`（還握著的租約）與 `unreadable`，`working` 只回報不阻擋（SPEC §18.10）。
  沒有這種核准時 `waited_secs`／`escalation_approval_id` 是 `null`，`safe` 的判準完全照舊。
  `approval=<id>`＝只看**那一筆**核准等了多久（acquire 一律這樣算，用它自己的 `approval_id`）；不帶才退回看最早那筆還活著的。
  認不得、已消耗、被撤或過期的 id 不放寬也不報錯。
  `owner=<name>`（CLI `--owner`）＝以這個人的身分問：**他自己握的租約不算擋**（只在升級時有差）。`held_leases` 每筆 `{resource,owner,own,fence,expires_at}`，
  `own:true` 就是發問者自己的。不帶 owner＝舊行為，每一把都算擋、`own` 一律 false；回傳的 `owner` 回聲讓呼叫端分得出舊 daemon 忽略了它。
  acquire 一律以自己的 `owner` 問（SPEC §18.10「自己的租約不擋自己」）。
- `GET /api/supervisor/leases` → `{leases:[{resource,owner,approval_id,fence,target_commit,acquired_at,expires_at,released_at,held}]}`。
- `POST /api/supervisor/leases/{rebuild|restart}/acquire {owner,approval_id,commit?,ttl_secs?,require_idle=true,exclude_bot_ids?}` → `{lease,lease_token,approval,safety}`；同 lock 內重驗核准與 idle，
  搶輸 409 `lease_held`。`owner` 必須等於核准的 `requester`，否則 409 `approval_owner_mismatch`（別人的核准開不了你的窗口，也借不走它的等待）。
  這張核准開過的窗口**過期沒 release**（執行端掛了）時，同一張再 acquire → 409 `approval_already_used`，並當場標 `consumed`（note `lease expired`）；要再開就重新申請。
  ttl 預設 900、上限 3600。`POST …/renew {owner,fence,ttl_secs?,lease_token}`、`POST …/release {owner,fence,lease_token}`；舊 fence 409 `lease_lost`；release 把核准標 `consumed`。
  **renew 不接受 `force`**（400）：force 只用來收掉持有者已經不在的窗口，不是替別人延長。
  **`lease_token` 只在 acquire 的回應裡出現一次**（`GET /leases`、`GET /api/supervisor`、WS 事件都不含它）。renew／release 不帶或帶錯 → `403 {"reason":"lease_token_required"|"lease_token_mismatch"}`，租約不動。
  強制接管：`{force:true, reason:"…"}`，**只有 AGM 角色**（`X-AM-Bot-Id`＋該 bot 的 hook token）可以；其他呼叫端 403 `lease_force_forbidden`。`reason` 必填（否則 400），不比對 owner／fence，寫進 `supervisor_notes` 的 `lease_force_release`（含 `by_role`）。升級前建立的租約沒有 token，不帶也能 release（見 SPEC §18.10）。
  持有 `restart` 租約期間 assignment 派送 hold（留 `queued`、不算重試）。

### 交辦 assignments（SPEC §18.8）
- `GET /api/supervisor/assignments` → `{assignments:[]}`。
- `POST /api/supervisor/assignments {target_bot_id,text,client_request_id,source_turn_id?,ownership?:[path],kind?:"task"|"notice",expects_review?,mission_id?,role?,review_role?:"patrol"|"responder"}` → 一筆 assignment（多 `review_role`）：
  `{id,target_bot_id,client_request_id,turn_id,status,text,delivery,result,error,attempts,request_id,created_at,updated_at,completed_at,turn_status,evidence_complete,open,awaiting_review,
  review:{decision,by,at,reason,followup_assignment_id},follow_up_of,legacy_closed,ownership,ownership_conflicts,resume_at,quota_retries,next_attempt_at,conflict_since}`。
  - `status`：`queued` | `delivered` | `unknown`（還在跑）→ `awaiting_review` → `completed` | `failed` | `cancelled` | `superseded`；另有 `blocked`、`quota_blocked`（都算未結案）。
  - 對方正在回合中：排成 `queued` turn，交辦停在 `delivered`＋`delivery="queued"`，推一則 `assignment_queued`（只記錄、不叫醒），不算失敗也不結案。
  - 一直送不進去（連續 409）：從 `conflict_since`（這一輪第一次 409；進 `quota_blocked`、被窗口 hold、送達時清掉）起超過 `AM_DISPATCH_CONFLICT_GIVE_UP_MINS`（預設 30 分）改成 `blocked`＋inbox 的 `assignment_undeliverable`
    （payload 帶 `conflict_since`、`reason`、`hint`；重派走 `review --decision followup` 配新的 `followup_request_id`，同一個 request id 再 assign 只會拿回這筆），
    **不是** `dispatch_failed`——工作沒失敗，是進不去（SPEC §18.8）。
  - `turn_status`（回合還在跑時 `null`）：`completed` / `completed_fallback` / `failed` / `dispatch_failed` / `turn_missing` / `quota_exhausted` / `identity_switch`。**回合結束不會自己變 `completed`。**
    `completed_fallback`（沒有回覆）結算之後遲到的 hook 補上回覆：`turn_status` 升成 `completed`、`evidence_complete=true`、`result` 補上，並推 `payload.late_reply=true` 的 `assignment_completed`（通知型是 `assignment_noticed`）（SPEC §18.8）。
  - `kind:"notice"`（或 `expects_review:false`，兩者都給時以它為準）：送達且回合正常結束直接 `completed`、inbox `assignment_noticed`；送失敗仍進 `awaiting_review`。CLI `agm assign --notice`。
  - `quota_blocked`：目標帳號被 CLI 擋著；帶 `resume_at`、`quota_retries`。額度回來後用 `<client_request_id>#r<n>` 自動重送，推 `assignment_quota_blocked` / `assignment_quota_resumed`；到期仍被擋時順延（不發通知）也算一次 `quota_retries`，累計 6 次 → `awaiting_review` + `quota_exhausted`。
  - `legacy_closed=true`：驗收狀態出現前就關掉的舊資料，未經驗收。
  - `ownership_conflicts`：前綴重疊的其他未結案交辦，只回報不阻擋。
  - `mission_id` 與 `role` 見「群組任務」。帶 `--mission` 時任務已經有一件開著的交辦 → 409 `mission_busy`
    （附 `requested_role` 與 `open_assignments[]`）：一個任務同時只有一件開著的交辦（SPEC §18.14）。
    退回／換手走 `review followup`，那條在同一個交易裡把原件標成 `superseded`，不受這道閘門影響。
    跟上面 `ownership_conflicts` 的差別：那個是字串比對猜的，只回報；這個是查得到的事實，所以擋。
    `role` 是 `reviewer`／`verifier` 而這一代還沒有被接受的執行成果（第一次派工、或 `round` 退回之後還沒重做）→ 409 `out_of_order`
    （`{requested_role, stage, generation, allowed_roles, next}`）；`executor` 在任何一關都派得出去（重做、rebase、沒做完再派）。
  - 先落地再送 prompt；同 `client_request_id` 重試回同一筆（換 bot 或 text 409）。對方忙 → 留 `queued`，`error` 記真正理由
    （`bot has no active run`、`a turn is already in flight`、`needs_login: …`），`next_attempt_at` 下次重試時間，controller 依 15/30/60/120/300 秒退避、沿用同一 crid。delivery `unknown` 只對帳不重送。
  - 送出的 user message 寫入時帶 `relay_from` = 總管 bot id（驗收角色是已建立的協調者時為協調者 id）；daemon 自己送給 AGM 的通知帶 `daemon`。
  - `review_role`：回報進哪個角色的 inbox。省略 = 呼叫的角色（`X-AM-Bot-Id`+`X-AM-Bot-Token` 驗證）；沒有角色 token = 協調者。followup 沿用。
  - **目標是另一個角色 bot** → 這是交接不是交辦：不建交辦列、不開回合，回 `{kind:"handover",routed,queued,duplicate,wake,inbox_event_id,delivery:"queued",turn_id:null}`（同下方 bot 申請的形狀）。對自己的角色 400。
  - `source_turn_id` 只能是**呼叫的那個角色自己**的回合（帶 bot token 時）；沒帶 token 的呼叫端可以指兩個角色之一的回合。
    省略時只認呼叫者自己在跑的回合，認不出呼叫者就記 `assignment_text_fallback`（不猜另一個角色的回合）。
- `GET /api/supervisor/assignments/{id}` → 單筆加 `reviews:[{id,decision,from_status,to_status,actor,source,reason,evidence,followup_assignment_id,created_at}]`。
  `mission_id` 指到的任務被**使用者**暫停時 409 `mission_paused`（daemon 自己設的暫停——`max_rounds`、`no_fable_for_verifier`、`push_main_failed`／`pr_failed`、`clarify`——不擋，runbook 要 AGM 在那些狀態下繼續處理）。
- `POST /api/supervisor/assignments/{id}/review {decision,actor?,source?,reason?,evidence?,followup_text?,followup_request_id?,followup_bot_id?,ownership?}` → 更新後的 assignment（`followup` 時另含 `followup`）。
  帶角色 bot token 時 `actor` 以 token 為準（`AGM:<role>`）。
  **唯一的結案路徑**。`accept`→`completed`、`fail`→`failed`、`cancel`→`cancelled`、`block`→`blocked`、`followup`→原本 `superseded` 並以 `followup_request_id` 另開 `follow_up_of` 的新交辦（不改寫已送出的 text）。
  同 decision 重送冪等；followup 重送須同 request ID、文字與目標，不同 409 `followup_mismatch`。`followup_request_id` 已經是別件交辦的 → 409 `followup_request_id_taken`（`{client_request_id,assignment_id}`，換一個 id）。
  續作沿用父交辦的 `expects_review`（通知的續作仍是通知）、`review_role` 與任務連結。已結案 409 `already_closed`；還在跑只接受 `cancel`（409 `still_executing`，且 cancel 不中止回合）。
  交辦掛在群組任務上時回應多 `mission_next: {mission_id, next, flow}`：裁示之後任務的下一步（同 `GET /api/missions/{id}` 的 `next`／`flow`）。
  決定成 `cancelled`／`superseded`／`failed` 時，交辦名下還在 `queued` 的 turn 一併撤銷（標 `failed`、插 system 訊息、釋放 queued 名額），回應多 `revoked_turn_id`；已經 `in_flight`／送出的不動、也不帶這個欄位（SPEC §4.4a）。
  這一刻撤不成（讀不到交辦、或撤銷寫不進去）時改帶 `revoke_pending_turn_id` 與 `revoke_pending`（說明）：那一則不會被送出，daemon 會在送出前再判斷一次並撤掉（#200）。
  撤回成功時，這次決定的稽核列（`reviews[].evidence`）也改寫成「排隊中的 turn … 已撤回，沒有送出」，不留「turn 還在跑」的警告。

### 交接、inbox、狀態、證據
- `GET /api/supervisor/handoff` → `{summary,summary_version,updated_at,requests,assignments,inbox,open_assignments,pending_count}`；`PUT {summary}` → `{summary,summary_version}`，同時寫 `handoff.md`（權威在 DB）。
- `GET /api/supervisor/inbox?all=0|1&limit=200&role=patrol|responder` → `{events:[{id,event_key,assignment_id,bot_id,turn_id,kind,payload,state,notify:{turn_id,delivery,attempts,next_at,error,delivered_at},role,wake,claimed_by,acked_by,merged_into,created_at,updated_at}],open,all,limit,role}`。
  預設只列未 handled、最舊在前；`all=1` 含已處理（最新在前）；`limit` 上限 1000；`role` 以 `COALESCE(claimed_by, role)` 在 **SQL 的 LIMIT 之前**過濾（先取一頁再過濾的話，最舊一整頁都是另一個角色時就翻不到自己的）。
  `POST /api/supervisor/inbox/{id}/ack` → `{}`；已結過 `{already_handled:true}`；帶角色 bot token 而事件歸另一個角色 → 409 `claimed_by_other_role`。
  - `role`／`wake`：SPEC §18.15 的路由表。`wake=false` 的事件不會自己開一次喚醒；`merged_into` 非空 = 被 daemon 合併掉（`acked_by:"daemon"`）。
  - `kind:"bot_request"`（`payload{to_role,wake,quiet_reason,from_bot_id,from_name,from_role,target_bot_id,text,client_request_id,attachments,sender_verified,via}`）：見下方「bot 寫給 AGM」。
  - `state`：`pending` → `delivered`（已送通知）→ `handled`（總管 ack）；另有 `gave_up`＝補送用盡、已停手（仍算未處理，ack 得掉，見下）。送達看 `delivery`：`failed` 留 pending 退避；`unknown` 綁 `notify.turn_id` 對帳不重送。
    delivered 但通知回合失敗／消失，或超過 `notify_ack_deadline_secs`（1800）沒 ack → 放回 pending；重送上限 `notify_max_attempts`（5，只算巡檢的事件），用完開 `notify_exhausted` incident（事件仍留著）。ack 單向。
    協調者**送不出去**的事件沒有次數上限：15 秒倍增退避到 `responder_max_backoff_secs`（300）；等額度時不計次。
    **送到了卻沒人 ack** 的補送兩邊都有上限（SPEC §18.15）：送達 5 次、或開著超過 6 小時且送達 ≥3 次 → `state='gave_up'`、不再補送，
    改推一則 `inbox_gave_up`（`payload{to_role,event_id,event_kind,owner_role,deliveries,open_secs,waiting_for,why,action}`）給**另一個角色**並叫醒它。
  - assignment 狀態遷移與完成事件同一個 transaction；啟動時補掃一次。
  - pending → delivered 的推送節流成每 `notify_interval_secs`（600）最多一次，一次併成一則通知；入庫不受影響。
  - `kind` 另有 `bot_restart_failed`（`batch_id,bot_id,name,error`）、`supervisor_restart_retry`（`batch_id,bot_id,name,ok,error`）、`approval_requested`、mission 相關事件（見群組任務）。
- `GET /api/supervisor/state` → 給 `agm` CLI 的精簡全域狀態：projects、bots（run 的 `agent_status`、`native_session_id`、`runtime_model/effort`、`pane_id`、`queued_turns`、`host_connected`）、未結案 assignment、待處理 inbox。不含 env、hook token、args、persona 全文。
- `GET /api/supervisor/evidence?q=<文字>&bot_id=&project_id=&before=<cursor>&limit=20` → `{messages:[{id,bot_id,bot_name,project_id,project_label,bot_deleted,turn_id,role,content,source,incomplete,created_at,truncated}],has_more,next_cursor}`。
  `q` 必填（trim 後 1–500 字），字面子字串（`%`、`_` 不是萬用字元）；`limit` 1–100；含已刪 bot 的歷史。依 `(created_at DESC,id DESC)`，`next_cursor` 原樣放回 `before`。
  每筆 content 最多 16,000 字（超過 `truncated:true`）。空查詢、過長、壞 cursor 400。只提供證據，不把命中當完成或適合度。

### bot 寫給 AGM（SPEC §18.15）
協調者建立後，下面兩條路目標是巡檢或協調者 bot 時**不開回合**，改寫 inbox `bot_request`：
- `POST /api/bots/{id}/prompt {text,client_request_id?,attachments?,relay_from:<bot id>,ack?,reply_to?}`（可帶 `X-AM-Bot-Token` 證明寄件者）→ **202**
  `{routed:"responder"|"patrol",queued:true,duplicate,wake,inbox_event_id,state,delivery:"queued",turn_id:null,message_id:null,note}`。
  沒有 `relay_from`（使用者）、`relay_from:"daemon"`、目標不是角色 bot、協調者未建立 → 照舊 200 `PromptOut`。
- `POST /relay/announce`（shim，`X-AM-Bot-Token`，表單 `bot_id,to_agent,text,ack?,reply_to?`）的 `to_agent` 對得上角色 bot（名字、agent 名、pane id）→ 200 同上形狀；shim 見 `routed` 不再轉給真的 herdr。其他 → `{}`（照舊記來源）。
- 去重：同寄件者同 `client_request_id` 一筆（結案了也是同一筆）；沒 id 時同寄件者、同內容指紋、同一個十分鐘格子、**還沒結案**的一筆——前一筆已 `handled` 就重新入列（新的 `inbox_event_id`、`duplicate:false`）。指紋 = 收件角色＋目標＋正文（逐字）＋附件。
  重複且指紋相同 → `duplicate:true`、同一個 `inbox_event_id`；指紋不同 → 409 `request_mismatch`（不寫入，回報既有事件 id）。
- `wake:false` 只在寄件端明講是回覆時：`ack:true`（`quiet_reason:"ack"`），或 `reply_to` 對得上一則跟寄件者有關的 inbox 事件（寄給它的角色、它寄的、收件角色寄來的）或派給它的交辦（id 或 `client_request_id`）（`"reply"`）。
  沒帶、或 `reply_to` 對不上（payload `reply_to_matched:false`）→ `wake:true`。不看寄件者當下在跑哪種回合。
  `POST /api/supervisor/assignments` 對角色 bot 的交接同樣收 `ack`／`reply_to`；對一般 bot 的交辦帶它們 → 400。

### `bin/agm`
`scripts/agm.py` 由 `include_str!` 編進 daemon，`setup` 時寫成 `<cwd>/bin/agm`。子命令：`state`、`supervisor`、`search`、`messages`、`bot`（`start`／`stop`／`restart`／`create`／`delete`）、
`assign`（含 `--notice`、`--mission`／`--role`、`--review-by patrol|responder`、交接用的 `--ack`／`--reply-to <event_id>`）、`assignments`、`inbox`（`--all`、`--limit`、`--role patrol|responder|mine`）、`ack`、`handoff`、`quota`、`health`、`lease`、`mission`、
`whoami`、`responder`（`show`／`setup`／`start`／`stop`）、`persona --role responder`；輸出一律 JSON。
執行期設定讀 `<cwd>/runtime.json`：`{daemon_url, manager_bot_id, responder_bot_id, bot_id, role, self_bot_id, data_dir, supervisor_id, remote_name}`（巡檢目錄的 `responder_bot_id` 在沒有協調者時是 `null`）；**沒有 token**，CLI 執行期 `GET /api/session` 取；`daemon_url` 只接受 loopback。
`role` 缺省＝`patrol`。環境 `AM_BOT_ID` 等於 `self_bot_id` 時，API 請求另帶 `X-AM-Bot-Id`／`X-AM-Bot-Token`（`AM_HOOK_TOKEN`）證明角色；mission 回報的 `relay_from` 用 `self_bot_id`。
設定目錄可用 `AGM_RUNTIME_DIR` 或 `--runtime-dir` 覆寫。

## 群組任務（mission，2026-09-13 新增，使用者決策見 SPEC §18.14 D1–D8）

群組裡的「交給 AGM」：使用者下一句指示，AGM 派執行者／reviewer／驗證者完成。daemon 只提供確定性的部分——
任務與事件的持久化、身分挑選規則、輪數上限、交付前的 fast-forward 檢查；拆工與判斷結果是 AGM 的事。
**第一版只支援本機專案**。

### 物件

```json
{ "id": "01M…", "project_id": "01M…", "client_request_id": "…", "text": "把設定頁的錯字修掉",
  "delivery_mode": "push_main" | "pr", "executor_kind": "claude" | "codex" | "grok", "on_5h_limit": "wait" | "switch",
  "max_rounds": 2, "rounds_used": 0, "paused_reason": null, "paused_detail": null, "result_summary": null,
  "status": "open" | "paused" | "done" | "cancelled",
  "created_at": "…", "updated_at": "…", "completed_at": null, "cancelled_at": null }
```

`status` 是從欄位算的（`cancelled_at` → `cancelled`、`completed_at` → `done`、`paused_reason` → `paused`、其餘 `open`）。
`GET /api/missions/{id}` 與清單多兩個欄位，都從該任務的 assignments 推導（不另存一份狀態）：

- `phase`：`done | cancelled | paused` 同 `status`；其餘看**最新一件還開著的交辦**——`executing`／`reviewing`／`verifying`（依 `role`），
  那件停在 `quota_blocked` 時是 `waiting_quota`；還沒有任何交辦＝`planning`；交辦都結案了但任務還開著＝`awaiting_agm`（輪到 AGM 決定下一步）。
- `assignments[]`：`{id, role, status, target_bot_id, turn_status, turn_error, follow_up_of, created_at, completed_at}`，舊的在前。
- `next`：daemon 從任務列、交辦與事件推出來的**下一步**（issue #74，`mission::flow`；規則見 SPEC §18.14）。
  `{action, role?, assignment_id?, retry_of?, rework?, sha?, worktree?, alternatives?, paused_reason?, then?, hint}`，
  `action ∈ assign | review | wait | record_verification | deliver | complete | paused | closed`。`alternatives` 是同一個判斷點上也合法的分支
  （`skip_reviewer`、`round`），選哪一條是 AGM 的判斷；`paused` 時 `then` 是放行之後那一步。
- `flow`：`{generation, stage, allowed_roles, verified: {event_id, sha, generation, stale}|null, delivered: {event_id, sha}|null}`。
  `generation`＝`round` 次數；`stage ∈ needs_executor | needs_review | needs_verification | needs_verdict | needs_delivery | delivered`；
  `verified.stale ∈ null | round | new_executor`（之後退回過、或又派了執行者，這則就放行不了交付）。
`paused_reason` 目前會出現：`max_rounds`、`no_fable_for_verifier`、`push_main_failed`、`pr_failed`、`user_pause`（任務卡的「暫停」按鈕），或呼叫端自己寫的原因。

### 交辦掛到任務上（P1b）

`POST /api/supervisor/assignments` 多收 `mission_id` 與 `role`（`executor | reviewer | verifier`），**兩個一起給或都不給**；
任務不存在 404、已結案 409 `mission_closed`。連結在派送**之前**寫入。follow-up（`review` 的 `followup`）自動沿用父交辦的
`mission_id`／`role`——撞限換手的接手工作就是靠這個接回同一個任務；任務已結案時 followup 一樣 409 `mission_closed`。
「任務還收不收新交辦」在 supervisor 鎖**裡**判定，`mission cancel`／`complete` 關任務那一步也拿同一把鎖：排隊等鎖的派工
不會在任務關掉之後冒出來（issue #119）。assignment 物件多 `mission_id`、`role`、`turn_error`
（回合結束時 run 上記的錯誤原因，例如撞限橫幅；`turn_status` 只說成敗）。

**任務裡的交辦撞到額度**時，controller 先照 `pick` 的規則判斷，再決定要不要走 §18.8b 的 `quota_blocked`：

- 同一身分、同一模型仍被挑中（例如 5h 撞限且任務選了 `wait`）→ 照原本的 `quota_blocked`，額度回來自己重送。
- 挑到**別的身分**，或同一身分但要**換模型**（Fable 用盡改 opus）→ 這件交辦進 `awaiting_review`，`turn_status = "identity_switch"`，
  AGM inbox 收到 `mission_identity_switch`（`{mission_id, assignment_id, role, bot_id, from_identity, to_identity, model, reason, message, needs_review:true}`），
  任務記一則 `note`。換身分不能續接 session（各身分的 `CLAUDE_CONFIG_DIR` 不同），所以由 AGM 用 `to_identity` 開新 bot、對這件交辦下 `followup` 接手，
  followup 文字要帶進度摘要（已做／未做／未提交檔案）。
- 驗證者找不到 Fable 有效額度 → 交辦進 `awaiting_review`（`turn_status = "quota_exhausted"`），任務停在 `no_fable_for_verifier` 等使用者決定。
- 不屬於任何任務的交辦，行為與先前完全相同。

事件（`GET /api/missions/{id}` 的 `events[]`，也是群組時間軸上這個任務的那一串）：
`{id, mission_id, kind, text, relay_from, payload_json, created_at}`，
`kind ∈ instruction | report | note | verified | round | paused | resumed | cancelled | delivered | completed`。
`round` 與 `verified` 的 payload 帶 `after_assignment`（寫下時這個任務最後一件交辦的 id，沒有就是 `null`），用來判斷之後的交辦屬於哪一代；
`verified` 另帶 `generation` 與給了的 `worktree`。`completed` 的 payload 帶 `delivery`（見 `complete`）。
`relay_from`：`null` = 使用者本人（只有 `instruction`），bot id = 那顆 bot，`"daemon"` = daemon 自己記的。

### 端點

| 方法 | 路徑 | 說明 |
|---|---|---|
| POST | `/api/projects/{id}/missions` | `{text, client_request_id?, delivery_mode, executor_kind, on_5h_limit, max_rounds?(0..=10，預設 2)}` → 任務＋`created`。同一個 `client_request_id` 回同一筆（`created:false`）。建立時記一則 `instruction` 事件，並往 AGM inbox 放一則 `mission_created`（event_key `mission:<id>:created`，payload 含 `mission_id/project_id/project/cwd/text` 與三個選項）；任務列、`instruction` 與 inbox 是**同一個交易**。重送（`created:false`）也補推一次 `mission_created`（同一個 event_key，已經有就不多一筆），寫一半的舊列靠它補回通知。遠端專案回 400 `{"error":"remote_not_supported","host":…}`。 |
| GET | `/api/projects/{id}/missions?status=all\|open\|done\|cancelled&limit=` | `{project_id, missions:[…]}`，新的在前。**已完成任務清單＝`status=done`，不含已取消的**；取消的另用 `status=cancelled` 取（UI 若要一起顯示須分開標示，不能混進「已完成」）。 |
| GET | `/api/missions/{id}` | 任務＋`events[]`＋`revisions[]`（這筆成果的續作，新的在前）＋`parent`（自己是誰的續作；來源被刪掉時是 `{id, missing:true}`）。 |
| POST | `/api/missions/{id}/events` | `{kind: "report"\|"note"\|"verified", text, relay_from?, payload?, worktree?, sha?}` → 事件。`relay_from` 規則同 `POST /api/bots/{id}/prompt`（不存在的值 400）。**交付前必須有一則 `verified`**，而且 `verified` 要說驗的是哪個 commit：`worktree`（本專案 repo 的工作樹，daemon 讀它的 HEAD）或 `sha`（可縮寫，必須是本專案 repo 裡的 commit），兩個都給時必須一致；daemon 把完整 sha 寫進 `payload.sha`。都沒給、工作樹不是本專案的 repo、sha 找不到 → 400。寫入時在同一個交易裡重看（驗 commit、算代都在交易外）：任務已結案 → 409 `already_closed`；`verified` 算好代之後任務被退回（`round` 先落地）→ 409 `verification_stale`（`stale_because: round`、`verified_generation`、`generation`、`next`）——驗的是上一代的成果，落地卻會被算成新一代的驗證。兩種都什麼都不寫。 |
| POST | `/api/missions/{id}/pause` | `{reason, detail?}` → 任務。`reason` **必填**（機器碼；缺了是 422，handler 不會跑）。任務列、`paused` 事件與叫醒協調者的 `mission_paused`（event_key `mission:<id>:paused:<event id>`，payload 含 `reason`／`detail`／`open_assignments[]`）是**同一個交易**；由收 mission 事件的那個 AGM 角色自己呼叫（bot token 驗過）時不推 inbox——自己叫醒自己只是多一個空回合。暫停**不**中止進行中的回合、不取消交辦，但 `deliver` 會 409（見下）。 |
| POST | `/api/missions/{id}/resume` | → 任務（清掉 `paused_reason`）。 |
| POST | `/api/missions/{id}/cancel` | → 任務，多 `temp_bots`（見 complete）與 `assignments[]`（被收掉的交辦：`{id, role, status, target_bot_id, turn_id, cancelled, revoked_turn_id?, may_still_be_running?}`）。任務列、`cancelled` 事件與 `mission_cancelled`（event_key `mission:<id>:cancelled:<event id>`；AGM 自己取消時不推）同一個交易；接著把底下**還開著的交辦**逐件走 `review --decision cancel`（排隊中的 turn 撤回、`quota_blocked` 不再被 controller 自動重送），最後才收臨時 bot。已經在跑的回合 daemon 不會中止，`may_still_be_running` 照實講。 |
| POST | `/api/missions/{id}/complete` | `{result_summary, relay_from?}` → 任務（`done`），多 `temp_bots: {deleted:[{bot_id,name}], skipped:[{bot_id,name,reason}]}`：結案時 daemon 自動軟刪這個任務的臨時 bot，條件是「任務某件交辦的目標」＋「名字以 `agm-mission-<任務 id 尾 6 碼（相容 5 碼）>-<角色>` 開頭」＋「沒有進行中的 run」；`reason ∈ not_a_temp_bot \| still_running \| state_unreadable \| delete_failed`。「確定沒有進行中的 run」才刪：讀不到 bot 列或 active run（DB 一時忙、I/O 錯）記 `state_unreadable`（帶 `detail`；讀不到 bot 列時 `name` 是 `null`），不呼叫刪除、不停機——之後 DB 好了照 `still_running` 的處置（先 `bot stop` 再 `bot delete`）。任務的 `note` 寫成「未刪除：名字（reason）」。刪除走 `DELETE /api/bots/{id}` 同一條路（停 pane、軟刪、子 agent 一起收、對話保留），並在任務記一則 `note`。 **關卡**：底下還有開著的交辦（`OPEN_STATES`）→ 409 `assignments_open`（附 `open_assignments[]`）：先 `review accept`／`fail`／`cancel` 收乾淨，或走 `mission cancel`（那條會自動逐件取消）。**對交付的要求**（body 另收 `no_delivery?: "no_changes" \| "user_declined"`、`worktree?`）：這一代驗過的 commit 已經交付 → 放行；沒交付就要 `no_delivery`，理由要對得上事實——`no_changes` 要任務從來沒有 `verified`，派過執行者時還要 `worktree`（執行者的工作樹，本專案 repo；乾淨而且 HEAD 已在 `origin/<base>` 裡，不 fetch），`user_declined` 要最近一次暫停之後有使用者本人（`relay_from` 空）的 `answer`。否則 409 `not_delivered`／`has_verified_changes`／`worktree_has_changes`／`user_not_asked`（都附 `next`），不認得的 `no_delivery` 或 `no_changes` 缺 `worktree` 是 400。判定結果寫進 `completed` 事件的 `payload.delivery` 並回在 `delivery`：`{status:"delivered", sha, mode, event_id}` 或 `{status:"waived", reason, answer_event_id?, worktree?, head?}`。任務列與 `completed` 事件同一個交易，只在這一次真的把任務從開著關掉時才寫：關卡跑完之前任務已被取消、或另一個結案先落地 → 409 `already_closed`，不留事件、不收臨時 bot。**關卡在寫入交易裡重判**（`round`／`verified` 不走 supervisor 鎖，鎖外算好的判定可能已經過期）：開著的交辦與交付要求照 commit 當下的交辦與事件再判一次，現在不成立 → 上面那些理由（例如判定之後被 `round` 退回 → `not_delivered`）；現在也成立、但跟先前判的不是同一件事（交付的是另一則 `delivered`、`no_changes` 現在要附 `worktree` 而先前沒要）→ 409 `mission_changed`（附 `decided`、`now`、`needs_worktree_proof`、`next`）。都不寫任何東西。 |
| POST | `/api/missions/{id}/question` | `{text, client_request_id, relay_from?}` → `{event, replayed}`。對成果追問，**已完成的任務也接受**。只寫 `question` 事件並推 AGM inbox（`mission_question`，`expects: answer_only`）；不 resume、不碰 `completed_at`、不建任務、不產生交付。AGM 代問時帶 `relay_from`，時間軸才看得出那句話是誰問的。 |
| POST | `/api/missions/{id}/answer` | `{text, client_request_id, reply_to?, relay_from?}` → `{event, replayed, resumed, mission}`。兩種語意由 `relay_from` 分：**沒有**（使用者本人）＝回答暫停的任務，daemon 在**同一個交易**裡寫事件、`paused→open`、推 inbox（`mission_answered`，event_key `mission:<id>:answer:<crid>`）；**有**（AGM／bot）＝回覆某則追問，`reply_to` **必填**且必須真的是這筆任務的 `question` 事件，不 resume、不推 inbox（自己叫醒自己就是通知迴圈）。使用者的 answer 只在任務真的 `paused` 時成立：`open` → 409 `not_paused`、`done` → 409 `already_closed`、`cancelled` → 409 `cancelled`，而且**被拒時什麼都不寫**（沒有事件、沒有 inbox）。 |
| POST | `/api/missions/{id}/revise` | `{text, client_request_id, relay_from?, delivery_mode?, executor_kind?, on_5h_limit?, max_rounds?}` → **新的一筆任務**（`parent_mission_id` 指回來，`created`）。原成果完全不動。沒指定的選項沿用原任務，**有指定就照 `POST /projects/{id}/missions` 同一套驗證**（enum 與 `max_rounds` 0..=10，專案還要存在且是本機）。新任務、它的 `instruction`、原成果那邊的 `note`、以及 inbox 是**同一個交易**：中途失敗不會留下沒人知道的續作。新任務的 `instruction` 事件與 inbox `mission_created` 帶快照（原指示／結果摘要／commit 或 PR／`verified` 摘要／新要求／`runbook_start_step: 2`），所以原本的臨時 bot 被清掉也不影響續作——快照是**參考，不是證據**，新任務仍要自己的 `verified` 才能交付。只有 `done` 能續作（進行中或已取消 → 409 `not_completed`）；同一個 parent 同時只能有一筆未結案的續作（第二筆 → 409 `revision_in_progress`，附既存那筆的 id），由 partial unique index 擋，並發也只會成立一筆。 |
| POST | `/api/missions/{id}/round` | 用掉一輪（review 退回或驗證失敗）→ 任務。`rounds_used` 與 `round` 事件（payload `rounds_used`、`after_assignment`＝寫下那一刻最後一件交辦）同一個交易，一起在或一起不在；請求途中任務已被關掉 → 409 `already_closed`。已達 `max_rounds` → 任務停在 `max_rounds` 並回 409 `max_rounds`（暫停與 `paused` 事件同一個交易；請求途中任務已被關掉 → 409 `already_closed`，什麼都不寫）。**使用者放行時上限加一**：停在 `max_rounds` 的任務被 `answer`（使用者回答）或 `resume` 放行時，`max_rounds` 設成 `rounds_used + 1`，`resumed` 事件寫「來回上限加一輪：N」、payload 帶 `max_rounds`——否則使用者說「再改一輪」也沒有路，AGM 一呼叫 `round` 又是 409、任務再停一次。加的是**一輪**，不是解除上限；別的原因停下來的放行不動上限。 |
| GET | `/api/missions/{id}/pick?role=executor\|reviewer\|verifier&exclude=<identity>` | 照任務設定挑身分，見下。`role=verifier` 回 `ask_user` 時會把任務停在 `no_fable_for_verifier`（任務這中間已被關掉就不停、不記事件）。使用者的身分停用清單讀不到 → 503 `policy_unavailable`（`retryable`、`retry_after_secs`）：不拿「都可用」挑（issue #160）。 |
| POST | `/api/missions/{id}/deliver` | `{worktree(本機絕對路徑), title?, body?, relay_from?}`。`push_main`：fetch → `origin/<base>` 必須是 HEAD 的祖先 → `git push origin HEAD:<base>`（fast-forward only，不 force）；`pr`：推 `mission/<id>` 分支並 `gh pr create`。`<base>` 是這個 repo 的預設分支（`origin/HEAD`，問不到才退回 `main`），回應與 `delivered` payload 都帶 `base`——寫死 main 的話預設分支叫 master／trunk 的專案一定 `fetch_failed`。成功記 `delivered` 事件並回 `{mode, sha, already_in_base}` 或 `{mode, branch, url, sha, existing_pr}`；**冪等**：同一個 commit 已經交付過就回原本那筆 payload＋`replayed:true`，不會再推一次。動手前記一則帶 `delivery_attempt:{sha,mode}` 的 `note`，所以「push／PR 成功但回應斷在路上」的重試認得出來——HEAD 已經在 `origin/main` 裡時回 `already_in_base:true` 並補記 `delivered`（沒試過那次仍是 `nothing_to_deliver`，那代表執行者根本沒 commit），PR 模式先問 `gh pr view`，已經有**開著的** PR（或已合併、而且 HEAD 已在 base 裡）就回 `existing_pr:true` 而不是 `pr_failed`；那條分支上被關掉的、或合併之後又有新 commit 的 PR 不算，照樣開一條新的（`gh pr view <branch>` 不看狀態，issue #132）。任務若停在 `push_main_failed`／`pr_failed`，成功時自動解除（記 `resumed`）。**關卡**（不改任務狀態）：任務停著（`paused_reason` 不是 `push_main_failed`／`pr_failed`）→ 409 `mission_paused`；`worktree` 必須是本專案 repo 的工作樹（否則 400）；沒有 `verified` 事件 → 409 `not_verified`；最新一則 `verified` 沒記 commit → 409 `verified_without_sha`；那一則之後有 `round`、或又派了執行者 → 409 `verification_stale`（`stale_because: round | new_executor`、`verified_generation`、`generation`、`next`；**HEAD 沒變也擋**，被退回的那一份不能靠舊驗證交付）；工作樹 HEAD 不是那個 commit（rebase、又改過、指到主樹）→ 409 `head_not_verified`（`verified_sha`、`head`）。過了關卡之後的失敗一律**停下來問人**（`push_main_failed`／`pr_failed`）並回 409，`reason` 是機器碼：`dirty_worktree`、`fetch_failed`、`not_fast_forward`、`nothing_to_deliver`、`push_failed`、`pr_failed`。交付途中任務已被取消 → 不停、不記 `paused`，回 409 `already_closed`（附 `delivery_failed`、`detail`）。 |
| PUT | `/api/identities/{name}/disabled` | `{kind, disabled, host?}` → 同一份。身分停用搬進 daemon（原本只在瀏覽器 localStorage）；WS `identity_prefs_changed`。停用＝群組任務挑身分與**環境設定／Bot 設定的身份選單**都看不到它（已經綁著它的 bot 仍看得到自己那一個），不影響執行中的 bot，也不動主機上的 alias。 |
| GET | `/api/identity-prefs` | `{disabled:[{host, kind, identity}]}`。 |

已結案（`done`／`cancelled`）的任務對任何**狀態變更**回 409 `already_closed`；`question` 與 AGM 的 `answer`
是例外（見上），因為對成果問一句話不會改變任何交付事實，擋掉只是讓使用者沒地方問。
每次變更推 WS `mission_updated {mission_id, project_id, status}`。

`question`／`answer`／`revise` 都要 `client_request_id`：內容相同的重送回原結果（`replayed: true`，
不會重複喚醒也不會開第二輪），**同 id 換內容**是 409 `request_id_reused`。
「內容相同」比的是**正規化指紋**，不是只有文字：事件看 `kind`＋文字＋來源＋`reply_to`，續作看
parent＋文字＋四個選項。只比文字的話，一則 `question` 與一則 `answer` 可以有同樣的字而被誤判成重送，
同一個 project 底下不同 parent 用同一個 crid 也會拿到別人的續作。指紋不含會變的快照，否則同一個請求
重送兩次會被判成兩個不同的請求。
重送判斷排在狀態檢查**之前**（而且兩者都在同一個交易裡）——重送多半發生在任務已經被放行之後，
先看狀態會把正確的重送擋掉；而放在交易外先查，兩個同 crid 的並發請求會雙雙通過檢查、其中一個撞索引變成 500。
`POST /resume`（不回答直接繼續）同樣會推 inbox（`mission_resumed`），但只在真的發生 `paused→open` 時推一次，
所以 AGM 自己呼叫 resume 不會把自己叫醒。`mission_resumed` 與使用者回答的 `mission_answered` 的 payload 都帶 `next`：放行之後的那一步。

**停在「輪到 AGM」的接續**（`mission_next`）：任務沒暫停、沒有開著的交辦、`next.action ∈ assign | record_verification | deliver | complete`，
而且任務與它的交辦最後一次變動超過 10 分鐘、這個任務也沒有沒處理的 inbox → controller 推一則 `mission_next`（協調者、叫醒），
payload `{mission_id, project_id, text, message, action, next, flow, idle_since, idle_minutes}`（`text`／`message`／`action` 是喚醒摘要會印的三欄：哪個任務、停多久、下一步），event_key `mission:<id>:next:<動作>:<角色>:g<代>:a<交辦數>:<commit 前 12 碼>`
——同一步只推一次，daemon 重啟後算出同一個 key，不會再推。

### CLI 對照（`bin/agm mission …`）

總管只能用 `bin/agm` 操作（沒有的子命令不自己拼 curl），所以每個端點都有對應：

| CLI | 端點 |
|---|---|
| `agm mission list --project <id> [--status all\|open\|done\|cancelled] [--limit N]` | `GET /api/projects/{id}/missions` |
| `agm mission get <mission>` | `GET /api/missions/{id}` |
| `agm mission events <mission>` | 同上，只取 `events[]` |
| `agm mission event <mission> --kind report\|note\|verified --text …（或 --text-file）[--worktree <驗過的工作樹> \| --sha <commit>] [--as-daemon]` | `POST /api/missions/{id}/events`（`verified` 沒給 `--worktree`／`--sha` 在送出前擋下） |
| `agm mission pause <mission> --reason <碼> [--detail …]` | `POST /api/missions/{id}/pause` |
| `agm mission resume\|cancel\|round <mission>` | `POST /api/missions/{id}/resume`／`cancel`／`round` |
| `agm mission complete <mission> --text …（或 --text-file）[--no-delivery no_changes\|user_declined [--worktree <執行者的工作樹>]] [--as-daemon]` | `POST /api/missions/{id}/complete`（`--no-delivery` 只收這兩個值，打錯在送出前擋下） |
| `agm mission question <mission> --text … --request-id <穩定鍵>` | `POST /api/missions/{id}/question` |
| `agm mission answer <mission> --text … --request-id <穩定鍵> [--reply-to <event id>]` | `POST /api/missions/{id}/answer`（預設帶 AGM 的 `relay_from`，回覆追問須 `--reply-to`；依使用者明確指示代送暫停回答須 `--as-user`） |
| `agm mission revise <mission> --text … --request-id <穩定鍵>` | `POST /api/missions/{id}/revise` |
| `agm mission pick <mission> --role executor\|reviewer\|verifier [--exclude <identity>]` | `GET /api/missions/{id}/pick` |
| `agm mission deliver <mission> --worktree <絕對路徑> [--title …] [--body …] [--as-daemon]` | `POST /api/missions/{id}/deliver` |
| `agm assign … --mission <mission> --role executor\|reviewer\|verifier` | `POST /api/supervisor/assignments` 的 `mission_id`／`role` |

`event`／`complete`／`deliver` 預設帶 `relay_from = runtime.json 的 manager_bot_id`（群組時間軸上顯示成總管說的）；
`--as-daemon` 改標 `daemon`。`runtime.json` 沒有 bot id 時直接報 `bad_args`，不送出——沒帶來源的回報會被當成使用者本人說的。
`--mission` 與 `--role` 只給一個也在送出前擋下。HTTP 錯誤照舊是非 0 結束碼＋`{"error":"http_error","status":…,"detail":…}`。

### 身分挑選（`pick`）

回傳 `{mission_id, role, kind, pick}`，`pick.decision` 是其中之一：

- `use` `{identity, model, reason}`：用這個身分；`model` 有值時要換模型（執行者／reviewer 撞到 Fable 週桶＝`"opus"`，驗證者一律 `"fable"`）。claude 以外的 kind 沒有身分可輪換，`identity` 是空字串。
- `wait` `{identity, until, reason}`：原地等這個身分重置（5h 撞限且 `on_5h_limit=wait`），或所有身分都用盡時等最早回來的那一個。
- `ask_user` `{reason, resets:[{identity, resets_at}]}`：停下來問使用者（驗證者找不到 Fable 有效額度）。
- `no_independent_reviewer` `{reason}`：沒有跟執行者不同的身分可以當 reviewer，改走「執行者自審＋驗證者把關」。

規則（`daemon/src/mission/pick.rs`，每一條都有測試）：claude 身分固定照 **cc2 → cc1 → cc0**，**用盡才換**——
`low` 不算用盡；**7d** 用盡（`critical`，或 `limit_hit` 推定為週窗）→ 換下一個；**5h** 用盡 → 照 `on_5h_limit` 等或換；
**Fable 週桶**用盡 → 執行者／reviewer 同一身分改用 opus，驗證者不能用這個身分。`limit_hit.bucket` 有值（claude 橫幅、開機回填從 park 時的 `error` 找回）
就照它決定是哪一種封鎖，時間取 `until` 與那一桶 `resets_at` 較早者，只有 5h／7d 讀數本身見底時才升級成那一桶；沒有桶名才從當下的桶子讀數推；過了 `until` 就不算。
讀不到額度視為可以用（未知不等於用盡），但驗證者例外：必須讀得到 Fable 桶且未見底。停用的身分一律跳過，
**這台主機上沒有的身分**也一律跳過（那台的身分表已經偵測過、而且至少有一個同 kind 的身分時才算數——
以前照樣回 `use`，AGM 開 bot 才被 409 `identity is not known on this host` 擋下）。

### OB 本機 CLI（網頁 GPT）

OB 不新增 daemon HTTP endpoint；`python3 scripts/ob.py ask --project-id <id> --request-id <stable-id> "問題"` 透過既有 `GET /api/supervisor/state` 核對 project ID，排入本機 SQLite 佇列，由單一 Sonnet-low worker 操作專案固定 ChatGPT 對話。`status <id>` 回 `{id,project_id,request_id,status,answer,url,error,...}`；只有 `done` 表示網頁原文已保存。`waiting_quota` 留待恢復、不切回 Fable；`unknown` 用 `collect` 對帳，不盲目重送。`recover` 回 `{worker_running,recovered[]}`：僅在 worker lock 可取得時把孤立的 `running` 轉成 `unknown`；不重新送出。`status` 也會執行此恢復，無 id 的回應附 `recovered[]`。完整狀態、安裝與舊 label 登錄轉 ID 的明確綁定方式見 [OB 操作文件](CHATGPT-CONSULT.md)。
