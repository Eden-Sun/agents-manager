# Agents Manager 規格書

> 本檔只寫**目前的行為**與讀程式看不出來的理由；演進過程看 git log。API 契約在 `API.md`，
> UI 取捨在 `UI-DECISIONS.md`，前端約定在 `FRONTEND.md`。

## 1. 目標

本機執行的多 agent 管理器：使用者透過 Web UI 以聊天方式管理在終端裡跑的 coding agent CLI——**Claude Code**、**Codex**、**grok**（§12）。

agent 一律由 **herdr**（terminal workspace manager，socket API protocol 20）承載。本系統不直接 spawn agent，而是透過 herdr 的 Unix socket 建 pane、啟動 agent、送訊息、讀輸出、訂閱狀態事件。

## 2. 名詞與資料模型

| 概念 | 說明 | 主鍵 | herdr 對應 |
|---|---|---|---|
| **Project** | 以目錄為單位的分組，canonical path 唯一 | `project_id`（`[A-Za-z0-9_-]{1,64}`，缺省產生 ULID） | 一個 `workspace`（記 `workspace_id`；對帳發現不存在就設 NULL，下次啟動 bot 時重建） |
| **Bot** | agent 設定：暱稱 `name`、kind、`model`、`effort`、args…，屬於一個 Project | `bot_id`（同上規則，永久） | — |
| **Run** | Bot 的一次執行。**每個 Bot 最多一個 active Run（DB 部分唯一索引）**。含 `agent_name`、`native_session_id`、`transcript_path`（hook 回填） | `run_id`（ULID） | `pane_id` + herdr agent name = `agent_name(project.label, bot.id)` = `<label slug>-<id 尾 6 碼>`（與暱稱無關） |
| **Conversation** | 與 Bot 1:1，跨 Run 延續，不拆分 | `conversation_id` | — |
| **Turn** | 一次「prompt → 回覆完成」。**每個 active Run 最多一筆 in-flight** | `turn_id` | Claude `prompt_id` / Codex `turn-id` |
| **Message** | `role ∈ {user, assistant, system}` | `message_id` | — |

bot `name` 是可隨時改的暱稱（不需重啟、允許 CJK，禁空白與 `@ , : ;`），唯一性在專案內。

### 2.1 Turn 狀態機

```
status:   in_flight ──► completed            （hook 配對成功）
              │    └──► completed_fallback   （終端備援；之後不被 hook 覆蓋，UI 標「可能不完整」）
              └───────► failed               （agent_blocked / interrupt / stop / 使用者放棄）
delivery: pending → ok | unknown | failed    （agent.prompt RPC 的結果，獨立於 status）
origin:   web | external                     （external = 非本系統送出、由 hook 或快照得知）
```

- 「進行中」= `status = in_flight`（不論 delivery）；`completed_fallback` 不算。
- `delivery = unknown` 時禁止再送 prompt，只允許 `interrupt`、`stop` 或 `POST /turns/:id/abandon`——否則下一則 hook 會配錯回合。

### 2.2 Bot 狀態（UI 呈現）

| 面向 | 值 | 來源 |
|---|---|---|
| 連線 | `connected` / `disconnected`（daemon ↔ herdr socket） | daemon |
| Run 生命週期 | `stopped` / `starting` / `running` / `stopping` / `exited` | daemon |
| Agent 狀態 | `idle` / `working` / `blocked` / `unknown` | herdr `AgentStatus`（`done` → `idle`） |

燈號：disconnected 灰；stopped/exited 離線；starting 黃閃；stopping 黃；running+idle 綠；+working 藍動畫；+blocked 紅；+unknown 灰黃。

## 3. 架構

```
React 前端 (Vite) ◄── REST + WebSocket ──► Rust daemon (axum) ◄── Unix socket (JSON lines) ──► herdr server (session=agents-manager)
                                              │  herdr client · registry/state · conversation store      │ panes
                                              │  hook receiver ◄──── hook / notify HTTP ──── claude / codex / grok
                                              └─ SQLite + config.toml（~/.config/agents-manager/）
```

### 3.1 daemon（`agents-managerd`）

- axum + tokio + serde；SQLite 用 sqlx（每條連線 `foreign_keys=ON`、`journal_mode=WAL`）。
- **DB schema 只支援現行版本**：`db::migrate` 只跑 `CREATE … IF NOT EXISTS`（加上 supervisor／mission 各自的 migration），不升級更舊的檔案；
  已移除功能留下的表與欄位（`teams`、`team_*`、`bots.team_id`…）在既有檔案裡原樣保留、不讀。
- **權威劃分**：TOML 是 Project／Bot 期望設定的唯一權威；SQLite 存 Run／Turn／Message／Conversation／hook token／workspace 映射。啟動與每次寫回 TOML 後做 TOML→SQLite 投影（依 id upsert；TOML 移除的 bot 標 `deleted_at`，保留歷史）。
- **投影不得大量軟刪**（2026-09-14 事故）：一次要軟刪的 bot／專案超過 3 列、或超過現有的 30%（兩列以上才算），或 config 裡一個專案都沒有而 DB 還有列 → **在任何寫入之前**拒絕整次投影並記 `error`，daemon 不啟動。
  啟動與 runtime 的**每一次**重投都走閘門：`ConfigStore::update` 會在磁碟 mtime 變了時重讀，「外面把 TOML 換掉／清空，再由 API 或總管觸發重投」是同一條事故路徑。
  DB 的活列＝上一次投影的結果，所以「config 空了但 DB 還有列」必然是拿錯 config／被換掉的檔案。
  閘門擋下來時 API 回 **409 `projection_refused`**（帶會被軟刪的 bot／專案名字與 `AM_ALLOW_BULK_DELETE` 提示），不是 502——
  502 的定義是「herdr／DB 出錯」，呼叫端分不出「你的設定沒被套用」跟「ssh 斷了」，而且之後每一次寫設定都會再撞一次（review 2026-09-16）。
  唯一的例外是明確的刪除 API（`DELETE /api/bots/:id`、`DELETE /api/projects/:id`），走 `projection::delete_from_config` 的單一臨界區：
  **重讀 config → 確認目標此刻在 TOML（不在就 409 `not_in_config`）→ 從當下的 TOML 算出實際要拿掉的 id → 閘門（寫檔前）→ 寫 config → 投影**。
  刪除模式的閘門是嚴格的：除了這次拿掉的 id，只要還有任何一列會不見就 409 `delete_refused`，不套小量門檻、不吃 `AM_ALLOW_BULK_DELETE`。
  所有投影與刪除共用同一把鎖，兩支 DELETE 並發時不會互相把對方的刪除當成未授權、也不會替對方放行。
  **先定案、再停機**：`DELETE /api/bots/:id` 全程拿著該 bot 的 per-bot 鎖（start 用同一把，拿到時 bot 已刪 → NotFound），會 409 的只有定案那一步、那時什麼都還沒停；
  定案後才停 child 與自己、軟刪 child、清目錄，所以停機期間 TOML 再怎麼變都不會留下「已停、未刪」（child 由母 agent 開、daemon 重開不了，事後回滾本來就做不到）。
  `DELETE /api/projects/:id` 依 id 排序拿齊專案內每顆 bot 的 per-bot 鎖，**在鎖內**重驗都已停止再定案；TOML 裡多出沒鎖住的 bot（剛建立、可能正要啟動）就 409 `delete_refused`。
  **鎖順序**：刪除是唯一會同時持多把 per-bot 鎖的路徑，兩支 DELETE 都「依 id 排序、一次拿齊」（`DELETE /api/bots/:id` 拿 parent＋所有 descendants，拿鎖途中若認領了新 child 就全放掉重來，三次後 409 `children_changed`），再用 locked 版停機；持一把再補拿另一把會與另一支互等成死鎖（ULID 不保證 parent 比 child 小）。
  其他情況真的要刪這麼多就 `AM_ALLOW_BULK_DELETE=1` 放行一次。
- **資料目錄隔離**：資料目錄依序取 `[server] data_dir` > `--config` 所在目錄 > `AM_DATA_DIR` > `~/.config/agents-manager`。非預設的 `--config` **一定**把 SQLite／`ui-token`／spool 帶到設定檔旁邊，不沿用預設目錄；`AM_DATA_DIR` 與算出來的不一致就拒絕啟動並說明。
- **同一資料目錄只准一顆 daemon**：啟動時對 `<資料目錄>/daemon.lock` 拿 `flock(LOCK_EX|LOCK_NB)`（拿到才寫自己的 pid 進去），拿不到就拒絕啟動、**不做任何寫入**（重啟時前一顆還在收攤，最多等 5 秒再判定失敗）；鎖綁在 fd 上，行程死掉自動放開（`startup.rs`）。
  順序是**唯讀解析設定 → 建立資料目錄 → 拿鎖 → 才允許建立目錄／寫檔**：`ConfigStore::load` 在設定檔不存在時會寫一份預設 config，那是共用設定，還沒拿到鎖的程序不能碰。拿鎖後重讀的設定若把 `data_dir` 改掉也拒絕啟動（拿著 A 的鎖寫 B 的 DB）。
- **資料目錄要跟著 bot 與 hook 走**：`hook.sh`／statusLine／grok dispatcher 的 argv 一律寫死 `--data-dir <解析後的資料目錄>`，本機 pane env 另外注入 `AM_DATA_DIR`（`pane_env`），herdr shim 也往子 agent 傳。
  argv 優先於 env：pane env 只保護這顆 daemon 新開的 pane，daemon 重啟前就存在的 pane 換不掉 env，但這些檔案每次啟動都重寫。遠端 pane 不注入 env（bot 目錄在遠端家目錄，§11.4）。
- **隔離實例不認領既有 pane**：資料目錄非預設時，reconcile 不把既有 agent／子 agent 收編成自己的 Run，`default_session` 的收編整個跳過，並記 `error` 要人在這顆 daemon 底下重啟那顆 bot——那些 pane 的 hook 指向別顆 daemon 的資料目錄，收編只會讓兩顆互相吃對方的 spool。
- **遠端也要分實例**：遠端的 bot 目錄、`hook.sh` 裡寫的 spool 目錄、drain／scan 路徑是同一個根 `$HOME/.config/agents-manager[/instances/<slug>]/bots/<bot_id>`，`slug` 是資料目錄的短雜湊（正式實例沒有這一段：既有路徑、檔名，以及沒有 `AM_INSTANCE` 的舊 pane 行為都不變；dispatcher 內容多了實例閘門）。
  grok 的 dispatcher 也按實例分址（`<根>/grok-hook.sh`），hooks 檔名是 `agents-manager[-<slug>].json`（grok 會合併整個 hooks 目錄）；每支 dispatcher 只接自己實例的 pane：隔離實例的 pane env 帶 `AM_INSTANCE=<slug>`，正式實例不帶（升級前開的舊 pane 也沒有，照舊歸正式）。
  `AM_INSTANCE` 與 `AM_DATA_DIR` 是**保留變數**：identity.env、bot.env 合併之後才由 daemon 蓋回去（隔離實例設 slug／正式實例移除；本機設資料目錄／遠端移除），自訂 env 寫了也不算。
  child 建立線同樣保留：herdr shim 在會建 pane 的 `pane split`／`tab create`／`workspace create`（及 `pane new`）剝掉呼叫者自帶的 `--env AM_INSTANCE=…`／`--env AM_DATA_DIR=…`（含 `--env=` 寫法），再照母 pane 的實際值補，母 pane 沒有就不帶。
  `agent start` **只剝不補**：herdr 0.8.2 的 `agent start` 沒有 `--env`（它在既有 pane 裡開 agent，env 在建 pane 時已注入），補上去就是未知旗標；`--` 之後是 agent CLI 自己的參數，原樣保留。
  `worktree create/open` 也會開 workspace，但沒有 `--env`：那個 root pane 由 herdr server 開、拿不到任何 `AM_*`，hook 不會觸發（dispatcher 要 `AM_BOT_ID`＋`AM_HOOK_TOKEN`），所以不會送錯實例，只是不被追蹤。
- **路徑解析不猜**：`normalize` 逐段 canonicalize，只有「這一段真的不存在」才當成還沒建立的尾巴；dangling symlink、symlink 迴圈等解析失敗一律拒絕啟動，不會被下一個 `..` pop 掉而錯映到別的目錄。
- **herdr client**：
  - socket：`~/.config/herdr/sessions/<session>/herdr.sock`；每個 RPC 一條新連線，送一行 `{"id","method","params"}`、讀一行回應。
  - 事件訂閱是長連線：**一條全域**（`pane.exited`、`pane.closed`、`workspace.closed`、`pane.agent_detected`）+ **每個 active Run 一條**
    `pane.agent_status_changed`（必須帶 `pane_id`，Run 結束時關）。事件行 `{"event","data"}`，名稱點號／底線兩種寫法都要認。
    斷線指數退避重連，重連後對帳（§6.5）。
  - 連上先 `ping`；`protocol != 20` 只警告。未知欄位與事件容忍；`docs/herdr-schema.json` 為契約參考。
- **session 管理**：socket 連不上就 spawn `herdr --session <name> server`（detached），輪詢最多 10 秒。daemon 退出不停 herdr。
- **per-bot 鎖**：每個 `bot_id` 一把 `tokio::sync::Mutex`；start／stop／prompt／hook 配對／spool 重放／對帳都在鎖內。
- **hook receiver**：`POST /hook/claude|codex|grok`，驗 per-bot token → 入佇列立即回 200 → 背景配對（§6.7）。
- **自動關滿意度問卷**：Claude Code 的 `How is Claude doing this session?` 會讓 agent 停下來等人，與工作無關 → 認出畫面一律送 `0`
  （`tui_prompts`）。進 `blocked` 當下看一次，另每 10 秒巡 `blocked`／`idle` 的 Run；額度探測 pane 也用同一套（那裡的 Enter 會變成替使用者評分）。
  其他等人回答的畫面一概不動。
- **claude 更新通知**：自動更新後 claude 只在 pane 最底印 `✔ Update installed · Restart to update`，不是事件。`update_watch` 每 30 秒
  對 running 的 claude run `pane.read visible 80`，認到就寫 `runs.update_notice` 並推 `bot_status`，消失就清 NULL（讀不到畫面不清）；不限 idle。
  認法（`tui_prompts::update_notice`）：兩段字都要中，**且只看最下面 6 行非空白**（正文引用這兩句時會誤中）。存在 run 上：重啟（套用更新本身）後的新 run 本來就沒有。
  畫面上讀不到那句時退到版本比對：statusLine 的 `version`（process 在跑的）對 `claude --version`（磁碟上的），磁碟較新才算；磁碟版本每台主機快取 5 分鐘。

### 3.2 前端

Vite + React + TypeScript + Zustand，只做 daemon 狀態的投影；正式版 rust-embed 進 daemon。約定見 `FRONTEND.md`，取捨見 `UI-DECISIONS.md`。

## 4. 回覆擷取

### 4.1 主要來源：hooks / notify（每次啟動注入，不改使用者全域設定）

**Claude Code**：`--settings <abs>`，檔案 `~/.config/agents-manager/bots/<bot_id>/claude-settings.json`，註冊 `SessionStart` 與 `Stop` 兩個 hook，
command 為 `/abs/agents-managerd hook claude --bot <bot_id> --token <t> --port <port>`。`stop_hook_active = true` 的 Stop 忽略。
stdin：SessionStart 含 `session_id`、`transcript_path`、`cwd`；Stop 另含 `prompt_id`、`last_assistant_message`、`stop_hook_active`。
同一個設定檔另外固定寫：`outputStyle: Concise`、`skipDangerousModePermissionPrompt`、`remoteControlAtStartup`（§18），以及 `timeFormat: "24-hour"`＋`timeZone: "Asia/Taipei"`
（使用者 2026-09-15：CLI 畫面裡的時間一律台北時間 24 小時制；claude 2.1.257 起才認，本機與遠端同一份）。

**Codex**：`-c notify=["/abs/agents-managerd","hook","codex","--bot",…,"--token",…,"--port",…]`；argv 最後一個參數是 JSON
`{"type":"agent-turn-complete","thread-id","turn-id","cwd","input-messages","last-assistant-message"}`。使用者原本的 `notify` 在此實例被覆蓋。

**grok**：沒有每次啟動的注入旗標，改用全域 hooks 檔 + env 分派，見 §12.2。

hook 身分是 **per-bot**（`bot_id` + `bots.hook_token`），daemon 解析該 bot 目前的 active Run：對帳收養會產生新 `run_id`，但存活的 agent 仍持有啟動時的參數。
pane env 的 `AM_RUN_ID` 只供診斷。

`runs.transcript_path` 由 SessionStart 回填；`messages.source` 保留 `transcript` 值（尚未實作回補）。

### 4.3 備援來源：終端快照

- **觸發**：Turn `in_flight` 且 `delivery = ok`，agent 由 `working` 轉 **`idle`**（`blocked` 不觸發），5 秒內沒收到 hook。
- **執行**：CAS `UPDATE turns SET status='completed_fallback' WHERE id=? AND status='in_flight'`，成功才 `agent.read {source: recent_unwrapped, lines: 200}`，
  取游標（`last_read_revision` + 已見文字尾端 hash）之後的內容，依 provider 抽回覆：Claude `⏺ ` 開頭、Codex `• ` 開頭；grok 無標記（§12.3）。
- **沒有回覆標記**時用 `clean_screen`：取最後一行 prompt 回音之後的內容，去掉 banner、方框、分隔線、狀態列、spinner、`⚠` 行，保留 `⎿` 工具結果行。
- **回音剝除**：畫面上只有 `❯ <第一行>` 算回音，多行 prompt 的其餘行逐行比對去掉。極窄 pane 下 TUI 會一列一個字、逐行比對必失敗，
  所以另有**去空白比對**後備：兩邊拿掉所有空白再比，候選開頭須是 prompt 的一段結尾（≥ 8 字元）。
- **認不出就說認不出**：剝完是空的 →「（終端沒有可辨識的回覆）」；剝完仍是一欄單字元（`is_shredded`：≥ 6 行且 ≥ 70% 行只有 1–2 字）→
  「（終端太窄，輸出被切成單字元而無法辨識；把 herdr 的 pane 拉寬一點就會恢復）」。不猜。
- 存成 assistant Message `source = terminal_fallback`、`incomplete = 1`。**晚到的 hook 不覆蓋**（去重丟棄並 log），避免跨回合錯配。
  **例外**：那筆 Turn 若一則 assistant Message 都沒有，晚到的 hook 是唯一答案 → 寫進去並把 Turn 改 `completed`（grok 思考時畫面就是空的 `❯`，
  備援會先收掉回合）。已有回覆的照舊丟；空 payload 不改狀態。
- **第二個觸發點**：`turn_progress` 輪詢器（狀態沒翻時的安全網：空輸入列且畫面沒變）。herdr 說 idle 且這回合**印過東西後停住** → 14 秒；
  herdr 說 working、或這回合**什麼都沒印過** → 63 秒（working 可能真的在想，等不夠的代價是吃掉使用者的問題）。grok 常駐 telemetry 橫幅不算內容。
  它也在 bot 鎖內呼叫同一支 `try_fallback`（在鎖外會與 hook 交錯成一回合兩則 assistant）；沒收成就繼續盯，Turn 被任一方收掉時迴圈自然結束。
- **沒有 hook 的 run**：被認領的 pane（`runs.adopted = 1` 且 `bots.inject_hooks = 0`，典型是 bot 自己開的子 agent，§6.5a）等不到 hook，
  快照是**唯一來源**：`working → idle` 沒有 in-flight Turn 時，補一筆 `origin = external`、`completed_fallback` 的 Turn（prompt 回音記 user、回覆記 assistant）。
  認領當下仍 `working` 就先開一筆 in-flight Turn。
  - 沒有游標時要求畫面上有 prompt 回音（否則整個 scrollback 變一則訊息）；擷取不到只推游標、不寫訊息。
  - 去重：游標 + 與上一則 assistant 比對（herdr 同一輪可能報兩次 idle；重啟會讀到同一畫面）；單則上限 6000 字；認領時的補記只在對話為空時做一次。

### 4.3a 回合被 API 中斷

claude 連線在回應中途掉了時，pane 只多一行 `⏺ API Error: Connection lost mid-response…` 然後收工——hook 照送 Stop、herdr 照報 idle，回合被記成
`completed`、側欄綠燈，使用者以為做完了。

- **觸發**：同一個 `working → idle` 邊、同一次 `recent_unwrapped` 讀取；備援有沒有出手都要跑。
- **判定**（`turn_error.rs`）：從畫面底部往上最多 30 行，剝框線與前導記號後，第一個非 chrome 行以 `API error`（不分大小寫）開頭，
  或是額度拒絕（`You've reached your Fable limit. Run /usage-credits …`）就命中。chrome = `is_noise`／`is_activity_shape` + 空輸入框列 + 更新通知行。
  額度拒絕認兩種前綴（CLI 2.1.271 的橫幅前綴表同時有 `You've hit your` 與 `You've reached your`）：`reached your … limit` 照舊；`hit your … limit` 只認速率桶。
  標成用完的桶照字面分：`session limit`→5h、`weekly`／`Opus limit`／`Sonnet limit`→7d、`Fable`→Fable 桶，認不出才先 5h 再 7d。
  `hit your monthly spend limit`、`fast limit`、團隊預算不是速率桶用完，不當撞限（2026-09-15；以前非 Fable 一律記 5h，撞週額度會把 5h 釘滿、等 5h 重置就當成解除）。
  關鍵是「最後一件事」：`API error · Retrying…` 之後又把答案講完的是重試成功，不算。
- **記錄**：原文寫 `runs.turn_error`（屬於這個 CLI 程序，重啟即清），對話補一則釘在該回合的 `system` 訊息（`incomplete = 1`、附快照）；
  回合還 `in_flight` 就收成 `failed`（不然輸入框鎖死）。同一行只記一次。
- **清除**：下一回合開始（`arm_progress`）設回 NULL 並推 `bot_status`。
- UI：側欄「⚠ 中斷」、標題列紅 chip，點開看原文與「重送上一則」（走既有 prompt API）。

### 4.4 hook 子命令（`agents-managerd hook claude|codex|grok`）最低契約

1. wall-clock ≤ 3 秒；**永遠 exit 0、永遠空 stdout**。
2. stdin（claude、grok）上限 1 MiB，超過截斷標 `truncated`；codex 取 argv 最後一個。
3. POST `http://127.0.0.1:<port>/hook/<provider>`（寫死 IPv4 loopback、`NO_PROXY=127.0.0.1`、連線逾時 300 ms、總逾時 2 秒），header `X-AM-Bot-Token`，
   body `{bot_id, provider, payload, received_at}`。
4. 失敗 → `O_APPEND` 追加一行到 `~/.config/agents-manager/bots/<bot_id>/hook-spool.jsonl`；寫失敗只記 `hook.log`，仍 exit 0。
5. `--port` 取自 command 列；env `AM_PORT` 為備援。
6. daemon 重放 spool：拿 per-bot 鎖 → rename 成 `.replaying` → 逐行照 §6.7 → 刪檔 → 放鎖。
7. **遠端 bot 不走 HTTP**：改成「寫 spool + `herdr pane report-agent`」，spool 是唯一內容通道，重放由 herdr 狀態事件觸發（§11.4）。

### 4.4a 模型／強度／fast：runtime 與設定

`bots.model` / `effort` / `fast` 是**設定**，不等於 bot 現在真的在跑的東西。

| kind | 執行中改 | 怎麼套用 |
|---|---|---|
| claude | 可以 | `apply_live_setting` 送 `/model <alias>`、`/effort <level>`；有對話紀錄時 `/model` 跳「Switch model?」確認框，daemon 回讀畫面按 `1`，關不掉就 Esc 並回 `needs_restart` |
| grok | 可以 | `/model <id> [effort]`、`/effort <level>` |
| codex | 可以 | `/model` 兩層選單 + `/fast` 開關，見下 |

`PATCH /api/bots/{id}` 只在**真的送不進去**（agent 忙、回合在飛、沒 pane、選單不對、回讀對不上）才回 `needs_restart: true`；此時 UI 不可顯示新設定。

- **daemon 記 runtime**：`start_inner` 在 `agent.start` 前用 `models::model_effort_from_argv` 從最終 argv 讀回，存 `runs.runtime_model/effort/fast`
  （讀 argv 而非抄 bots：`effort_checked` 會丟掉模型不收的等級，`bot.args` 也可能自帶 `-m`）。slash 指令套用成功就同步改。
- **收編的 pane**三欄是 NULL = 不知道，UI 不比也不標。codex 例外：它把三個值印在狀態列上，reconcile 讀那行補 NULL（`reconcile::fill_codex_runtime`）——
  否則 UI 會拿 bots 頂上，而 `/fast` 是開關，不知道的 tier 等於切不掉。
- **UI 一律顯示 runtime**；設定 ≠ runtime 時多一顆「需重啟」chip（`POST /api/bots/{id}/restart`）。不准靜靜顯示還沒生效的值。
- **codex 的 fast 兩個方向都送**：少送 `service_tier` 等於「聽 `~/.codex/config.toml`」，而那裡常寫著 `fast`。所以一律帶 `-c service_tier="priority"`（勾）或
  `-c service_tier=""`（沒勾）。`priority` 是 `model/list` 唯一廣告的 tier，TUI 顯示為 `fast`。model／effort 不這樣做：它們的「不指定」在 UI 上就寫「使用 CLI 預設」。

**codex 即時套用**（`codex_live.rs`）：
- `/model` **不吃參數**（帶參數會被當 prompt 送出）。空 `/model` + Enter → `Select Model and Effort` 編號選單 → 選模型 → `Select Reasoning Level` → 選強度，
  印 `• Model changed to …`。`Max`/`Ultra` 在 `More reasoning…` 底下。只改強度也要先選模型（沒指定時選 `(current)`）。
- `/fast` 是開關，只在現有 tier ≠ 目標時按；`runtime_fast` 為 NULL 時讀狀態列。PATCH 的 live 欄位閘門要把 `fast` 算進去。
- **選單一律用讀的**：號碼、順序、`(default)`/`(current)` 會跑；每步回讀 pane，比對「號碼後到兩個空白為止」的 label（說明文字會含別的模型名）。
- **最後回讀狀態列**（`<model> [<effort>] [fast] · <cwd> · Context …`）確認；`runtime_*` 存讀到的值，對不上回 `needs_restart`。
- codex 會把選擇存成帳號預設（寫 `~/.codex/config.toml`），是 CLI 行為。
- **套不進去要說得出是哪一步**（2026-09-13）：`apply_live_setting` 回「原因」而非布林，每個出口寫一行 log，
  並經 `PATCH` 的 `live_apply` 回給呼叫端（docs/API.md §10.2）。在這之前失敗是靜默的，只剩
  `needs_restart: true`，使用者問「改 effort 為什麼又重啟」時 log 裡沒有任何線索。0.154.0 的兩層選單原文
  釘成測試 fixture（`codex_live` 的 `MODEL_MENU_0154` / `EFFORT_MENU_0154`）：選單改字會讓這條路**靜靜**
  退回重啟，讓測試先講。
- **旁註：被重啟掉的 codex 怎麼接回原對話**——沒有「重啟並續接」的 API（`/restart` 無 resume 旗標、
  `restart-idle` 只吃帶更新的 claude）。繞路是把 `bots.args` 暫時設成 `["resume","<上一個 native session>"]`、
  `/restart`、確認接上後再還原 `args`：codex 的 resume 是**子命令**，start 時 args 排最前面，形狀剛好是
  `codex resume <id>`（2026-09-13 對 GPT-astra 實作過）。

## 5. 設定檔

`~/.config/agents-manager/config.toml`：

```toml
[server]
listen = "127.0.0.1:7788"
herdr_session = "agents-manager"
# data_dir = "/tmp/am-iso"          # 留空＝跟著這份設定檔所在的目錄；相對路徑也以它為準

[supervisor]
notify_interval_secs = 600

[[projects]]
id = "01JABC1234567890XYZ1234567"   # 缺省時首次載入自動補寫
path = "/Users/me/project/foo"
label = "foo"

  [[projects.bots]]
  id = "01JABC1234567890XYZ1234567"
  name = "foo-claude"
  kind = "claude"                   # claude | codex | grok
  args = ["--model", "opus"]        # 接在 daemon 注入參數之後
```

- 未知欄位用 `serde_ignored` 收集並 WARN 完整路徑，仍可載入。
- **設定檔在哪，資料就在哪**：`--config` 指到非預設路徑時，SQLite／`ui-token`／spool 一律跟著設定檔的目錄（或 `[server] data_dir`），不得沿用 `~/.config/agents-manager`；`AM_DATA_DIR` 與它不一致就拒絕啟動（§3.1）。隔離測試請用
  `agents-managerd serve --config /tmp/am-iso/config.toml`（要跑 hook 就再 `AM_DATA_DIR=/tmp/am-iso`，兩者必須一致），**不要**只換 `listen` port：2026-09-14 就是這樣開到正式 DB，被空 config 投影軟刪了 15 顆 bot。
- 寫回：serde 全量序列化（註解不保留），暫存檔 + 原子 rename，單一 mutex；mtime 與上次讀取不符時在同一把 mutex 內重讀再套用。內容沒變就不碰檔案。
- 改 `listen` port 需重啟 daemon，既有 agent 的 hook 會打舊 port（靠 spool + 對帳補入）。
- `[supervisor] notify_interval_secs`（預設 600）：事件照舊即時寫入 `supervisor_inbox`；被節流的只有「喚醒總管」——每 ≥ 這個秒數一次，
  把累積的未 ack 事件彙整成一則 `[AG Man 通知]`。health 偵測、watchdog、控制器 TICK 不受影響。總管 busy 時延後，送成功才開始下一個視窗。
  `0` = 不節流。上次喚醒時間存 `supervisors.last_notify_at`。改值要重啟 daemon。只管巡檢；協調者見 §18.15。
- `[supervisor] responder_batch_secs`（預設 15）、`responder_max_backoff_secs`（預設 300）：協調者的短窗批次與重試上限（§18.15）。

## 6. 生命週期

**送 prompt 的路徑（2026-09-14，sol review 後定案）**：daemon 一旦直接對某個 run 的 pane 打過字
（當場套用 slash、codex 選單、`/login`），就把 `runs.pane_typed` 設為 1；herdr `agent.prompt` 在這種
pane 上回過 ok 卻沒送進去（wits-c1-op-xh 14:24、15:33，第二次距 slash 兩分鐘）。之後這個 run 的 prompt
一律改成打字進 pane，`agent.get` 查不到 `agent_session` 綁定的 agent 也走這條（`lifecycle/delivery.rs`）。

**證據在打字之前就決定**。畫面上 TUI 畫出來的文字不能拿來重建 prompt（軟折行與使用者按的換行在畫面上一樣、
行尾空白看不見、tab／emoji／組合字元／ZWJ 的寬度不可靠），所以不做任何還原。無損證據有三種：

- **claude transcript**：本機、run 有 `native_session_id` 且 `transcript_path` 檔案存在。
- **codex rollout**：本機、run 有 `native_session_id`（codex 第一次回合結束時回報），且在該 bot 的
  `CODEX_HOME`（identity env → bot env → `~/.codex`）底下 `sessions/` 日期樹（由新到舊整棵走）找到檔名恰為
  `rollout-…-<session id>.jsonl` 的檔案；同一天有多個取修改時間最新的，canonicalize 後必須仍在 `sessions/` 底下。
  session 已知但 rollout 還沒寫出來時先不打（`NotAttempted(codex_log_not_ready, retry)`）：直接送的回 409，排隊中的
  放回等 3 次後才退回一列回音或 unverified。等待次數另存在 `turns.rollout_waits`，綁 `turns.rollout_wait_key =
  <run id>:<session id>`：只有這個原因會累計（框忙等其他放回不算），run 或 session 換了就從頭算。只算頂層 `response_item`／`message`／`role=user` 且全部是
  `input_text` 的項目；`compacted` 重播的歷史、developer 訊息、帶圖片的訊息都不算。
- **一列回音**：單行、首尾無空白、不含 tab／控制字元／ZWJ／變體選擇符／組合字元，且在 herdr `pane.layout`
  回報的當下欄寬下保證放得進一列（ASCII 一欄、其他兩欄保守估，加 marker 與 6 欄餘裕）。送出後輸入框上方要多出
  恰好一列 `❯ <原文>`（claude 也接受 `> `；codex 是 `› `），原樣前綴比對、不 trim，且底下沒有續行。

兩種 session log 都是：打字前記下檔案長度當基準，送出後只讀基準之後新增的位元組，要多一筆與送出文字**逐位元組
相同**的 user 訊息；同時 run 仍須指向同一個 session（claude 另比對路徑），途中換了就是 `Unproven("session_changed")`；
檔案比基準還短視為讀取錯誤。

**證據矩陣**（先符合的先用）：

| provider | 主機 | 單行、放得進一列 | 其他（多行、長文、縮排／行尾空白、特殊字元、量不到欄寬） |
|---|---|---|---|
| claude | 本機，transcript 已回報 | transcript | transcript |
| claude | 本機，hooks 開著但 transcript 還沒回報 | 一列回音 | 不打，`NotAttempted(transcript_not_ready, retry)` 等 SessionStart |
| claude | 本機，hooks 關閉 | 一列回音 | **unverified** |
| claude | 遠端 | 一列回音 | **unverified** |
| codex | 本機，找到 rollout | rollout | rollout |
| codex | 本機，session 已知但 rollout 還沒寫 | 先等（409／排隊放回 3 次）→ 一列回音 | 先等 → **unverified** |
| codex | 本機，session 還不知道 | 一列回音 | **unverified** |
| codex | 遠端 | 一列回音 | **unverified** |
| grok | 任何 | 一列回音 | **unverified** |

超過 20 萬字一律 `NotAttempted(prompt_too_long_to_prove)`（422），不打。

**「有沒有證據」與「能不能自動重送」是兩件事**（AGM 2026-09-16 裁示）：
- `turns.delivery_verified` 只講**證據**：有沒有無損證據證明它進了對方的輸入框／session。
- `turns.auto_resend` 只講**能不能自動重送**：打過字但證不明的那條路重送會重複派工，所以是 0；
  `agent.prompt` 同樣沒有證據，但沒送進去才會走到重送，所以是 1。重送閘門看 `auto_resend`，不看 `delivery_verified`。
- 欄位 additive、migrate 可重入；既有列 `auto_resend` 預設 1，行為與拆開前相同（舊的 unverified 列當時已把
  `resend_count` 頂到上限，照樣不會被重送）。
- **UI 只標沒人補救的那一種**（2026-09-16）：turn JSON 同時帶 `delivery_verified` 與 `auto_resend`；
  無證據＋會重送 → 只在 hover 說明，無證據＋不重送 → 畫「未驗證送達」。理由與實作見 UI-DECISIONS 與 `web/src/lib/deliveryNotice.ts`。

**結果五種，對呼叫端意義不同**：
- `Submitted`：打字進 pane，而且有無損證據證明送出。verified=1、可重送。
- `Handed`：交給 herdr `agent.prompt`。它回 ok 卻不保證字進得去（2026-09-14 wits-c1-op-xh 實例），
  所以**沒有證據**：verified=0、API 回 `"delivery":"unverified"`、UI 標「未驗證送達」；重送照舊允許。
- `Unverified`：沒有無損證據可用，照樣打字送出；框收下貼上、Enter 後清空，就回報成送出，但標成「要人工核對」。
  DB 存 `delivery='ok'`＋`turns.delivery_verified=0`（`delivery` 的 CHECK 只有原本四種狀態，不改表），API 回
  `"delivery":"unverified"`，AGM 交辦記成 `delivery=unverified`，UI 在使用者泡泡上標「未驗證送達」。它照常掛 stall
  與進度輪詢、Enter 補送，但**絕不自動重送**（可能已經被收下）。
- `NotAttempted`：一個字都沒送。
- `Unproven`：按過鍵、該有證據卻證明不了——只有這種會變成 `delivery='unknown'`。

直接送出的 prompt 在建立 turn **之前**先規劃（路徑、證據、空框），`NotAttempted` 不建 turn：可重試的回 409（AGM 交辦
維持 queued 退避），不可能的回 422。規劃後、打第一個字前框才被填上的極小競態，把剛建的 turn 與 user 訊息刪回去再回
409，讓同一個 request id 能重送。排隊中的 prompt 的重試規則見下方「排隊中的 prompt 重試」。

**空框的判定依各 provider 的實機畫面**（`screen.rs` 的真 fixture 都有測）：

| provider | 輸入框長相 | 算空框 |
|---|---|---|
| claude（現行） | 兩條全寬 `───` 夾著 `❯` 那一列（實機是 `❯` 加一個不斷行空白） | `❯` 後什麼都沒有，或後面的可見字**全部畫成 dim**（佔位字 `Try "…"`、「建議下一句」等，內容不限）；下一列就是框線 |
| claude（舊版）、grok | `│ ❯ … │`，框底 `╰…╯`（grok 在框底寫模型：`╰── Grok 4.6 (low) · always-approve ─╯`） | `❯` 後只有框內的補齊空白；下一列就是框底 |
| codex | 沒有框的 `› …` 那一列 | `›` 後什麼都沒有，或後面的可見字**全部畫成 dim**（例如輪播的範例句 `Ask Codex to do anything`）；下一列不是縮排續行 |

佔位字與「有人打了同樣的字」在純文字快照裡一模一樣，所以判斷空框時一律用 `pane.read format=ansi` 讀：marker 後每個可見字元都
在 SGR `2`（dim）底下就是 TUI 自己畫的提示，不看字面內容（`38`/`48` 顏色參數裡的 `2` 不算）；讀不到樣式、只有空白、或任一可見字元非 dim，一律當成非空。
「建議下一句」是 **provider 行為**、不是穩定介面：claude 2.1.269 起中／日／泰文 prompt 也會出建議句（之前那幾種語言的建議被丟掉，是 CLI 修了 bug 才冒出來，
2026-09-14 的 dim 建議句 409 就是它，55deeb5 修）。內容、語言、出現時機以後還會變，所以只能看 dim 樣式判斷，不要加字面或語言的特例。
**codex 的點字動畫**（v0.154.0／gpt-6-astra 起）：輸入列與上下各一列撒著會動的點字 `⠁⠂⠄⠈⠐⠠⢀`（U+2800–U+28FF），每顆都帶自己的前景色、不是 dim，而且蓋在空白格上——包括 `›` 後那一格、草稿字與字之間的空格。只有 **codex＋styled 讀法**時，帶前景色、非 dim 的點字視同原本那格空白：marker 列擦掉點字後只剩 dim 內容或空白就是空框，下一列只剩縮排與點字就算空白列。沒有顏色或 dim 的點字、任何一般字元照樣是內容；純文字讀法不放寬（分不出是不是打的）；claude／grok 不套這條。真畫面 fixture：`daemon/src/lifecycle/fixtures/codex_astra_particles_{empty,draft}.ansi`。

herdr 回錯且訊息明確提到 `format`（舊版或遠端不認得這個參數）時退回純文字讀法——沒有樣式可看，佔位字自然判非空；herdr
接受參數卻回 `format: text` 時也照樣用，但每個 pane 每 10 分鐘最多記一次 warn，讓降級看得見。打第一個字**之前**的失敗——讀不到
畫面（`composer_unreadable`）、證據檔在規劃後被刪除或讀不到而無法建立基準（`transcript_unreadable`）——一律是可重試的
`NotAttempted`（直接送撤回 turn 回 409、排隊的放回），不會變成 502 或 `delivery=unknown`；只有 `pane.send_text` 之後的失敗才可能是 unknown。

marker 列與框的邊之間多出任何一列（含空白列）、marker 後多打一格（沒有框的輸入框）、dim 提示後面多了非 dim 的字、跟要送的一模一樣的字
——都是非空，一律不代送、零寫入。認不出框的畫面是 `Unready`。

**誰會建 `queued` turn**（2026-09-16 AGM 裁示）：**只有 AGM 的派工／通知**這條路
（`supervisor::controller::dispatch` → `lifecycle::prompt::prompt_relayed_queueable`）。對方回合中時它排一筆 `queued`
而不是 409——一顆回合 10～20 分鐘的 bot，用退避重試等於每五分鐘賭一次它剛好在兩個回合之間（實例：交辦
01M2MC8CB2AGDKPB86XDW1FB0Q 重試 12 次、42 分鐘都沒送出）。**使用者與 web 的 `POST /api/bots/{id}/prompt`
維持 409**，那條路的語意變更要單獨評估，不要照這段設計「送一次就好，daemon 會排隊」的使用者流程。

界線：每個對話最多一筆 `queued`（`turns_one_queued`），同一筆交辦重試回同一筆（`turns_client_req`），撞到就回 409 照舊退避；
排超過 `[supervisor] assignment_queue_wait_secs`（預設 1800 秒）還沒送出，controller 把交辦停在 `blocked` 並推一則通知；
daemon 重啟時把所有 `queued` turn（含沒有 `next_flush_at` 的）重新掛上 flush，不留孤兒。送出時機與證據記錄完全沿用下面這套。

**排隊中的 prompt 重試**：
可重試原因（框忙、transcript 還沒回報…）放回 `queued`，退避 15 秒起每次加倍、上限 5 分鐘；次數與
下次時間存在 `turns.flush_retries`／`turns.next_flush_at`，時間未到的其他喚醒不動它；每顆 bot 同時只有一個重試 timer；放回
12 次仍送不出就標 failed 並插說明（同一個 transaction）。daemon 重啟（`reconcile::rearm_progress`）時掃描所有帶
`next_flush_at` 的 queued turn，以 `max(now, next_flush_at)` 為每顆 bot 重建唯一的 timer。直接送出的 409 回應則由呼叫端（或 AGM
交辦的既有退避）重試。

`auto_resend=0` 的 turn 同時把 `turns.resend_count` 設到上限：就算退回不認得 `auto_resend` 的舊 binary，也不會被自動重打一次。
（反過來，`Handed` 這種 verified=0 但可重送的列，退回舊 binary 時會被舊的 `delivery_verified` 閘門擋著不重送——少送不會重複送。）

打字流程：空框 → 一次貼上 → 框變成非空（仍是空的且證據沒變才再貼一次；**沒有可讀證據的 `Unverified` 一律不貼第二次**——
那個判斷對它恆成立，會變成「框看起來空的就再貼」，遠端／grok 晚一幀重畫就送出兩段接在一起的文字）→ Enter → 框回到空的**且**證據比基準多一。

**重啟時卡在送出途中**（`in_flight` 而 `delivery='pending'`）：那一格的收尾者只活在上一個行程裡，開機時由 `rearm_progress` 收——
鍵可能已經按下去，所以標成 `unknown`（不是當成沒送，也不是把回合結掉），交給既有的放棄／人工判斷。
沒收的話那顆 bot 之後每則 prompt 都 409，而且 §18.10 的 safety 會一直把它讀成「正在送達臨界區」而擋住重啟窗口。
同一輪也會把**兩分鐘內剛送出**（`delivery='ok'`）的 in-flight turn 補回 stall watchdog；更舊的不補，否則 12 秒後會把舊訊息再送一次。

**撤回**（一個字都沒送出而刪掉 turn 與訊息）之後推一次 `resync`：事件模型沒有「刪除」，不補的話客戶端會留著一顆送不出去的泡泡與一個永遠不會結束的回合。
`NotAttempted`（零寫入）的重送要退還 `resend_count`：唯一一次補救機會不該被「框裡剛好有字」這種兩秒後就消失的原因吃掉。
框在 Enter 後仍有字就再按一次並繼續驗。任何讀取失敗都是錯誤，不是空畫面。`runs.pane_typed` 要先寫成功才碰 pane
（slash 與 prompt 都是），寫不進去就中止；行程內另有保守記號，讀不出來時當成「要打字」，不退回 `agent.prompt`。

stall watchdog 的自動補送走同一條驗證路徑，次數記在 `turns.resend_count`（每個 turn 上限 1，UPDATE 認領
即是鎖，queue flush 與 watchdog 不會各送一次，daemon 重啟也不會多一次額度）。

### 6.1 daemon 啟動
0. 決定資料目錄（§3.1：`data_dir` > `--config` 所在目錄 > `AM_DATA_DIR` > 預設）、對它拿 `daemon.lock`；
   `AM_DATA_DIR` 不一致或鎖被別人佔著就**在開 DB 之前**結束，不寫任何一列。
1. 載入 config、補寫缺少的 id、TOML→SQLite 投影（大量軟刪會被擋下，見 §3.1）。
2. 確保 herdr session 在跑、`ping`。
3. 對帳（§6.5）。
4. 建全域事件連線與各 active Run 的狀態連線。
5. 每個 bot 重放 spool。
6. `autostart = true` 且無 active Run 的 bot 走 §6.2。

### 6.2 啟動 Bot（per-bot 鎖內）
1. `INSERT runs (state='starting')`；違反 active Run 唯一索引 → 409 附既有 `run_id`。
2. workspace：`projects.workspace_id` 存在且 `workspace.get` 成功就用，否則 `workspace.create {cwd, label, focus:false}` 並更新映射。
3. 先做不需要 pane 的準備（hook 注入檔、CLI 參數；遠端可能 ssh 上傳）與執行檔 preflight。失敗 → Run `exited`，**不建 tab／pane**。
4. 取得 pane：workspace 剛建立 → 用 `root_pane`；否則 `tab.create {workspace_id, cwd, label: tab_label(bot), focus:false, env}` 取 `root_pane`。
   **一個 bot 一個 tab**，不 `pane.split`——共用 tab 會把 pane 越切越窄，窄到 TUI 一列一個字時備援完全讀不出東西（§4.3）。
   `env`：`AM_BOT_ID`、`AM_RUN_ID`（診斷）、`AM_PORT`、`AM_HOOK_TOKEN`（`inject_hooks = false` 時不給）、`CLAUDE_CODE_CHILD_SESSION=""`、`CLAUDECODE=""`。
   失敗 → Run `exited`、回 502。
5. 更新 Run 的 `workspace_id`／`pane_id`／`tab_id`；先寫 `runs.agent_name`，再 `agent.start {name, kind, pane_id, args: injected ++ bot.args, timeout_ms: 60000}`
   （立即回 `launch_pending`）。之後所有 herdr 目標一律用 `run.agent_name`。pane 建好到 start 成功之間任何失敗 → Run `exited` + 盡力關 pane（tab 空了一併關）。
6. 開該 pane 的狀態訂閱。
7. `agent.wait {until:[idle,done,blocked], timeout_ms: 60000}`：idle/done → running+idle；blocked → running+blocked（例如 trust 提示）；
   timeout/error → 不關 pane，`agent.get` 有 agent → running+unknown，沒有 → `exited` + 關 pane。

**tab 生命週期**：停止與 orphan 回收共用 `close_pane_and_tab`：先 `pane.close`，再 `tab.list` 確認該 tab `pane_count == 0` 才 `tab.close`；共享 tab 不動，
tab 已被回收視為完成，`tab.list` 失敗不猜。沒有 `tab_id` 的 Run 只關 pane。
`POST /api/bots/{id}/pane/move-to-tab`（`move_pane_to_own_tab`）把非獨占的 pane 搬到新 tab 並更新 `tab_id`；pane id、訂閱、進行中 Turn 不變，不重啟 agent。

### 6.3 送訊息（per-bot 鎖內，單一 DB 交易）
1. 冪等：先查 `client_request_id`，已存在 → 回同一 `turn_id`（200），不做後續檢查（即使已有新 Turn 在飛）。
2. 前置檢查：Run `running`；agent ≠ `blocked`；無 `in_flight` Turn；無 `delivery=unknown` Turn → 否則 409（body 含原因與既有 `turn_id`）。
3. `INSERT turns (in_flight, pending, web)` + user Message；commit；推 WS。
4. 鎖內 `agent.prompt {target, text}`（逾時 10 秒）：成功 → `delivery=ok`；`agent_blocked` → `delivery=failed`、`status=failed`；逾時／連線錯誤 → `unknown`（不重送）。
5. 完成靠 hook（§6.7）或備援（§4.3）。Turn 在送 prompt 之前已 `in_flight`，所以 hook 早於 RPC 回應也配得到。
6. `interrupt`（送 `esc`）／`stop` 把 in-flight Turn 標 `failed` 並加 system Message。
7. **stall watchdog**：`delivery = ok` 後 12 秒內沒收到 `working`／`blocked` 且仍 idle/unknown → 讀 `visible` 快照 → Turn `failed` + system Message，
   中性敘述並原樣引用含 `Not logged in`／`/login`／`unlock-keychain`／`usage limit`／`limit` 的行（提示 ssh 下 macOS Keychain 可能讀不到）。
8. 請求可帶 `relay_from`（bot id 或 `"daemon"`），記下這則是誰轉述的，UI 據此不把它算成使用者發言。

### 6.4 停止／刪除 Bot（per-bot 鎖內）
- `interrupt`：`agent.send_keys [esc]`，Run 狀態不變。
- `stop`：Run `stopping` → in-flight Turn 標 `failed` → `ctrl+c` ×2（間隔 500 ms）→ 等 `pane.exited` 或 agent 消失最多 10 秒 → 否則 `pane.close` → `stopped` → 關訂閱。
- `POST /bots/:id/restart`：有 Run 先 stop 再 start，用來套用改過的 model／args／identity／env。
- DELETE Bot：TOML 移除＋DB `deleted_at`（單一臨界區，§3.1；保留對話）→ stop（child 與自己）→ 刪 `~/.config/agents-manager/bots/<bot_id>/`（遠端 ssh `rm -rf`，失敗只 log）。
  先定案再停：拒絕只會發生在任何東西被停之前（2026-09-14 sol 四輪；原本是 stop 在前）。全程持該 bot 的 per-bot 鎖。
  `child` 不在 TOML，直接 `deleted_at` 並停 pane。
- DELETE Project：拿齊專案內每顆 bot 的 per-bot 鎖 → 鎖內確認都已停止 → TOML 移除；不關 workspace、不刪目錄。
  **child bot 一併軟刪**（鎖還在手上時做）：child 不進 TOML，投影的「不在 TOML 就軟刪」只管 user bot，
  不收的話會留下一批 `deleted_at IS NULL`、專案卻已軟刪的列——UI 看不到、reconcile 也掃不到（`live_bots_on_host` 要求專案還活著），
  它們的 pane 與 hook 目錄從此沒人回收（review 2026-09-16）。

### 6.5 對帳（啟動、事件連線重連；逐 bot 在鎖內）
1. `session.snapshot` + `agent.list`。
2. DB 的 active Run：清單上找得到它的 agent name → 維持，更新 `pane_id`（pane move 會改 id）；`agent_status` 在鎖內再 `agent.get` 一次
   （清單是拿鎖前讀的，用舊的 idle 蓋掉已 working 的 run，真正的 `working→idle` 就不會觸發備援與排隊 prompt）。找不到 → `exited`。
3. 清單上的 name 對得到某 bot 但 DB 無 active Run → 建 Run（`running`、`adopted=1`），沿用同一 Conversation。
4. orphan pane 回收：`exited/stopped` Run 的 `pane_id` 仍在 snapshot 且沒有 agent → `pane.close`。
5. 重建各 active Run 的狀態訂閱。

### 6.5a 子 agent 認領（血緣優先）

一個 bot 一個 tab。對帳的逐 bot 迴圈走完後，`agent.list` 裡**沒有 bot 認領**的 agent 依序試兩條線索：

1. **血緣（優先）**：它的 `tab_id` 等於某 bot 活動 run 的 `tab_id` → 那顆 bot 的子 agent（子 pane 從父 pane split 出來，必然在父的 tab 裡，不需要 agent 配合）。
   同一 tab 有多顆 bot（父 + 已認領的子）時取名字前綴最長者，平手取非 `child`——孫代因此掛在子代下面。
2. **名字前綴**：`<某 bot 的 agent 名>-<字尾>`，取最長匹配。跨 tab 只有這條。

兩條都中以血緣為準。認領：`managed_by='child'`、`parent_bot_id`、`adopted=1` 的 run；同一父 bot 底下同名的 live child 直接重用。
子 bot `name`：有前綴取字尾，否則用 herdr agent 名（去空白與 `@,:;`、截 32 字）。字尾在專案裡已被別人用掉時改存完整 herdr agent 名（herdr 保證唯一）。
每顆認領各自成敗：失敗只 log 跳過，不中止整台主機的對帳。

**子 agent 退役**：子 agent 只活在它的 pane 裡，pane 沒了就退役（`bots.deleted_at`，對話保留）。兩條路都要接：reconcile 發現 run 在、agent 不見；
以及 `pane_closed` 事件**先**結束 run、reconcile 後到——bot 沒有 active run、herdr 清單找不到它、且至少有一個已結束的 run，一樣退役。
herdr 還列著這個 agent（pane 被搬走）的不算，會被重新收編。`pane_closed` 結束的是子 agent 的 run 時，2 秒後自己排一次 reconcile。

### 6.5b herdr PATH shim（命名規則做成機制）

daemon 每次起 pane 前把 POSIX `sh` 包裝腳本裝到 `<bot 目錄>/bin/herdr`（遠端走 ssh），並放到 pane `PATH` 最前面。

- `herdr agent start <name> …`：`<name>` 不以 `$AM_AGENT_NAME-` 開頭就補前綴（截到 32 字）並在 stderr 說明。旗標可在名字前面，`--kind`/`--pane`/`--timeout` 的值不誤認，`--` 之後原封不動。
  **模型沿用**：`--` 之後沒有 `--model` 且 `--kind` 與母 bot 相同（或沒寫）時補 `-- --model $AM_MODEL`，claude 再補 `--effort $AM_EFFORT`；
  子 agent 自己寫的一律尊重（`--model`、codex/grok 的 `-m`、codex 的 `-c model=` / `-c model_reasoning_effort=`）。
- `herdr pane split` / `pane new` / `tab create`：原樣轉發並補 `--env`，帶下 `CLAUDE_CONFIG_DIR`、`CODEX_HOME`、`AM_BOT_ID`、`AM_HOOK_TOKEN`、`AM_PORT`、`AM_RUN_ID`、
  `AM_AGENT_NAME`、`AM_KIND`、`AM_MODEL`、`AM_EFFORT`、`PATH`——herdr 的 pane 是 **server** 生的、不繼承呼叫端 shell，沒這段子 pane 會用預設帳號起來、拿不到 hook token。
  呼叫端自己給的同名 `--env` 不動。
- `herdr agent prompt`：見 §6.5d。其他子指令 `exec` 真正的 herdr（`$AM_REAL_HERDR`，否則 `PATH` 上第一個不是自己的）。

**PATH 只靠 pane env 不夠**：herdr 用 login shell 開 pane，profile 之後才跑並重建 `PATH`（macOS `path_helper` + `brew shellenv` 會把 shim 擠到後面）。
所以 `agent.start` 前再對 pane 的 shell `pane.send_text` 一行 ` export PATH=<dir>:"$PATH"`。裝不起來不擋啟動：§6.5a 的血緣認領仍追得到。

子 agent 指定自己的 pane 用 herdr 注入的 `$HERDR_PANE_ID`（或 `--current`）。

### 6.5c 給 claude 注入 herdr skill

啟動 claude bot 前，把 `herdr --skill` 的輸出寫到該身份的 `$CLAUDE_CONFIG_DIR/skills/herdr/SKILL.md`（遠端用 ssh）；內容相同就不寫。寫之前改兩處：

1. frontmatter `description` 換成 AG Man 版（herdr 原文說「使用者明確提到才用」，對 AG Man 裡的 bot 剛好相反）。
2. body 最前面插 **AG Man 規則**（`lifecycle::child_agent_rules`）：先 `herdr agent list` 找自己底下閒置的 child 重用、命名、`herdr pane split --pane "$HERDR_PANE_ID"`、
   不要 `git stash`/`--autostash`、子 agent 會掛在自己底下、帳號與 hook 自動帶進子 pane；瀏覽器一律用 ego lite、一個 bot 最多一個分頁、結束就關。

herdr 的 CLI 說明原樣保留（升級會帶進新文字）。裝不起來只 warning。`child_agent_rules` 是同一份文字來源：claude skill 與三種 kind 的 persona
（`--append-system-prompt` / `--rules` / `developer_instructions`）都用它。

**語氣是規格的一部分（2026-09-13）**：注入給 bot 與 child 的人設／提示一律寫成**命令**——「必須」「一律」「禁止」，
開頭先講明「硬規則，不是建議」。客氣的寫法（「請…」「…比較清楚」）agent 會當成建議而不執行，實測就是這樣漏掉找閒置 child、
漏掉關分頁。`lifecycle` 的 `the_rules_read_as_orders_not_suggestions` 測試守這條：出現「請」就紅燈。

### 6.5d agent 對 agent 的 prompt 標出來源

走 daemon 的派工（`POST /api/bots/{id}/prompt` 帶 `relay_from`、總管的 assignment）會寫 `messages.relay_from`，UI 畫成「X → 這顆 bot」。
agent 自己 `herdr agent prompt <名字> …` 時 daemon 沒參與，那句話只以 prompt 回音從 hook 回來，會跟使用者打的字長得一樣。補法同 §6.5b，做成機制：

1. shim 攔 `agent prompt`：目標名 herdr 認得（`herdr agent get` 找得到：AGM、其他頂層 bot、pane id）就照原名送，找不到才當自己的子 agent 補前綴。
   決定名字後先 `POST /relay/announce`（表單 `bot_id`／`to_agent`／`text`，header `X-AM-Bot-Token` 用該 bot 的 hook token），再轉給真的 herdr。
   報不成功只是少一次標示；名字前面帶旗標時整串原樣轉發。
2. daemon 把「誰要送什麼給哪個 agent」記在行程內的短命表（5 分鐘）。
3. 回音從 hook 回來時用 run 的 `agent_name` 認領：忽略所有空白（TUI 任意折行），長度取兩邊較短者且至少 12 字元；更短就要完全一樣。
   認到就在**插入當下**寫 `relay_from`（事後補的話 `message_added` 已經推出去了）。
4. 認不出來維持 NULL = 使用者自己打的。寧可少標，不把使用者的話說成別人送的。

### 6.5.1 採用使用者的 Herdr `default` session

daemon 另外唯讀觀察本機 Herdr `default` session（`~/.config/herdr/herdr.sock`），不替它啟動 server。啟動、事件重連與定期輪詢時：

1. `agent.list`，只處理 `claude`/`codex`/`grok`。
2. agent 的 `foreground_cwd`（沒有用 `cwd`）與既有 local Project 的 canonical path **完全相等**才配對；不自動建 Project、不採用普通 shell pane。
3. 有採用紀錄就更新 Run；否則建 `herdr_session = "default"` 的 Bot 設定（寫回 config.toml）與 `adopted = 1` 的 active Run。
4. default workspace 不寫 `projects.workspace_id`；default pane 消失只結束 Run，不回收使用者 pane。`stop` 只送 ctrl+c、不 `pane.close`；
   `start`/`restart` 回 409 `default_session`；§6.9 批次重啟跳過。

default Bot 的 prompt／keys／terminal 讀取依 Run 的 session 回到 default socket；沒 hook 的 agent 靠 pane status 與終端備援更新對話。

### 6.6 事件處理
- `pane.agent_status_changed`：更新 `agent_status`；`working→idle` 啟動備援計時（§4.3）；推 WS。遠端 run 先 drain 一次該 bot 的 spool（§11.4.3）。
- `pane.exited` / `pane.closed`：Run → `exited`，in-flight Turn → `failed`。
- `workspace.closed`：`projects.workspace_id = NULL`，其下 Run → `exited`。
- `pane.agent_detected`：只 log。

### 6.7 hook 與 Turn 的配對（per-bot 鎖內）
1. 驗 token、解析 active Run；沒有 → external（第 5 點）。
2. 分類：
   - Claude `SessionStart` / grok `session_start` / Codex 首次任何事件 → 回填 `native_session_id`、`transcript_path`，**不建 Turn**。
   - Claude `Stop`（`stop_hook_active=false`）/ Codex `agent-turn-complete` / grok `stop`（`reason = end_turn` 且 `stopHookActive = false`）→ 配對。
   - 其他 → ack 丟棄。
3. 去重：`(native_session_id, native_turn_id)` 已存在 → 忽略。
4. 目標 = 該 Run **唯一**的 `in_flight` Turn：
   - 有 → CAS `… WHERE status='in_flight'`，**成功**才建 assistant Message（`source=hook`）、Turn `completed`、寫 native ids。
     CAS 輸了且 Turn 已 `completed_fallback` → 把 native ids 蓋上；已有 assistant Message 就丟 payload，一則都沒有就用 hook 的回覆補上並改 `completed`。
     被其他原因收掉的（stop／failed）照舊保留回覆。
   - 無 → 第 5 點。
5. external：建 Turn（`origin=external`、`completed`）+ user Message（Codex 取 `input-messages`；Claude 沒有就省略）+ assistant Message。
6. 推 WS `message_added` / `turn_updated`。

### 6.9 一鍵套用 claude 更新（批次 exit + resume）

claude 下載新版後只能靠重啟套用（`runs.update_notice`，§3.1）。

- 入口：`POST /api/bots/restart-idle`（無 body），**立刻回計畫**，重啟在背景跑（一顆 `stop_bot` 最久等 10 秒）。
- 挑選（`bulk_restart::plan`，純函式有測試）：候選 = kind 是 claude 且 run 帶非空 `update_notice`；非候選的連「跳過」都不列。候選依序判斷：

  | 條件 | `reason` | 動作 |
  |---|---|---|
  | run 或 bot 的 `herdr_session = 'default'` | `default_session` | 跳過 |
  | `runs.state != 'running'` | `not_running` | 跳過 |
  | `agent_status = 'working'` / `'blocked'` | `working` / `blocked` | 跳過 |
  | `agent_status` 不是 `idle` | `unknown_status` | 跳過 |
  | 還有 `in_flight` Turn | `turn_in_flight` | 跳過 |
  | 以上都不中 | — | 重啟 |

  刻意保守：批次最不能做的就是砍掉使用者正在等的回合。
- **執行**：序列、一顆一顆，每顆 `lifecycle::restart_bot_with(StartOpts { resume_native: true })`——stop 與 start 在**同一次持有 bot 鎖**裡做完
  （中間有空檔時，拿鎖前讀了 agent 清單的 reconcile 會搶進來把剛停掉的 agent 收編成新 run，start 就以 `active run already exists` 放棄，bot 從此沒人拉起）。
  start 被一個 pane 已不存在的 run 擋住時先結束那個 run 再試。`stop_bot` 寫上 `ended_at` 後剛結束的 session 成為「上一個 session」，claude 拿到 `--resume <session>`。
  與 `POST /bots/:id/restart` 的差別只有這個旗標（那條是重新開始）。
  - **沒寫過 transcript 的 session 不續接**：沒被 prompt 過的 claude `--resume` 會 `No conversation found` 立刻退出。本機 hook 回報的 `transcript_path` 不存在時改開新對話
    （`member_context_lost("transcript_missing")`）。
  - **子 agent**不能照一般路徑重開 pane，改走 `lifecycle::restart_child_in_pane`：送 `ctrl+c` 讓 agent 退出、**不關 pane**，同 agent 名在同 pane `agent.start`，
    帶 `--resume <上一個 session>`、bots 上的模型／強度與 `auto_approve` 旗標（pane shell 裡的帳號與 shim 不變）。過程中 pane 不見 → run 標 exited 不重開。
    agent 10 秒內沒退出 → 回 502、不動 pane，run 從 `stopping` **放回 `running`**（agent 還在）。單顆 `POST /api/bots/{id}/restart` 對子 agent 走同一條路。
  - 序列而非並行：per-bot 鎖與 pane 版面都假設一次一顆，並行的錯誤也分不出是誰的。
- **一顆失敗不中斷整批**。最後仍啟動失敗的：留下 pane 已關的 run 就結束掉，並推 supervisor inbox `kind = bot_restart_failed`（`batch_id`、`bot_id`、`name`、`error`）。
- **總管 bot 排最後**；重啟後 60 秒內每 5 秒檢查 running／pane／agent，沒回來就自動再啟動一次，推 `kind = supervisor_restart_retry`（含 `ok`、`error`）。
- **reconcile 的配合**：拿到鎖後若要**收編**或**改寫 run 的 pane**，先重新 `agent.get` + `pane.get` 確認，agent 不在或 pane 已關就不動（RPC 失敗維持原判斷）。
  有 active run 但 herdr **按名字**找不到 agent 時不能直接標 exited：herdr 以名字為鍵登錄，同名 agent 在新 pane 重開後，舊 agent 晚到的退出處理會清掉新 agent 的名字
  （`agent.list` 裡 `name: null`，`pane.get` 仍回 `agent: claude`）。所以改問 run 自己的 pane：`agent.get <pane_id>` 是它自己的 agent → 保留 run 並
  `agent.rename <pane_id> <name>` 補回名字；是別人的或空的才標 exited；RPC 失敗這輪不動。`run_alive` 同理以 pane 有沒有 agent 為準。
  herdr 仍列著 agent 的 `stopping` run 轉回 `running`。
- 回饋走 WS：`bots_restart_progress`（每顆 `restarting` / `ok`|`failed`）與 `bots_restart_done`（`ok`/`failed`/`skipped` 三張清單）。
- UI 入口在額度列 claude 量表右邊的 `⬆ N` chip（`UpdateQuotaChip.tsx`），確認框列出要重啟與跳過的；側欄 `UpdateAllBanner` 只顯示進度與失敗／跳過名單。

### 6.10 Fork 頂層 bot（接續對話脈絡）
- 入口：側欄 bot 列 `⋯` →「開同類分身…」→ 選「接續對話（fork）」→ `POST /api/bots/:id/fork`（`daemon/src/fork.rs`）；選「全新對話」走原本的建 bot＋啟動。只給頂層 bot：child 的 pane 與帳號環境是母 agent 開的，daemon 重建不出來；從 default session 匯入的也不行（§6.5.1）。
- 來源 session：來源 bot 最近一次有 `native_session_id` 的 run（跑著的也算——三家 CLI 的 fork 都是讀對話檔，不打擾原本那顆）。本機對話檔不在就拒絕，避免 CLI 找不到對話直接退出。
- 新 bot：config.toml 條目照抄來源（同一個 identity／env 才找得到那段對話），`autostart=false`，名字預設 `<來源>-fork`，**插在來源正下方**（陣列位置＝側欄順序；使用者 2026-09-15）。建好後以 `StartOpts.fork_session` 啟動一次：
  claude、grok 附 `--resume <id> --fork-session`；codex 的 `fork` 是子命令，`fork <id>` 排在所有參數最前（三家 CLI help 2026-09-14 實測）。不寫 `runs.resume_session_id`：fork 本來就會拿到新 id，不能當成 resume mismatch。
- 之後兩顆各走各的：新 bot 的下一次重啟用它自己的新 session（照 §6.9 的 resume）。分叉前的訊息不複製到新 bot 的對話紀錄（CLI 裡有），改在新 bot 對話放一則系統訊息指回來源。
- 跟「開同類分身並啟動」的差別只在脈絡：分身是全新對話。

## 7. API

完整契約在 `API.md`；這裡只記存取控制與 WS 語意。

### 7.1 存取控制
- bind：開發版 bind `0.0.0.0`；打包成 macOS app 的執行檔（路徑在 `…app/Contents/MacOS/`）bind `127.0.0.1`；`AM_DEV_LAN` 可雙向覆寫（`main.rs::dev_lan_default`）。
- 啟動時產生 UI token 寫 `~/.config/agents-manager/ui-token`；`GET /api/session`（`Host` 須為 `127.0.0.1:<port>` 或 `localhost:<port>`）回 token；
  其餘 `/api/*` 要 header `X-AM-Token`，`/ws` 用 `?token=`；`Origin` 存在時須為本機。
- `/hook/*` 與 `/relay/announce` 驗 **per-bot** `X-AM-Bot-Token`。

### 7.3 WebSocket `/ws`
- 事件帶遞增 `seq`（記憶體，daemon 重啟從 0）。客戶端帶 `?since=`；daemon 保留最近 200 則，補不齊或 seq 倒退 → `{"type":"resync"}`，客戶端重新 `GET /state` 與訊息。
- 事件種類見 `API.md`。終端畫面由前端輪詢 `GET terminal`，不走 WS。

## 8. 技術選型
- Rust：axum 0.8、tokio、serde/serde_json、sqlx 0.8（sqlite）、toml、ulid、clap、tracing、reqwest（hook 子命令）、rust-embed。
- 前端：Vite、React 19、TypeScript、Zustand、自寫 CSS。
- 結構：`daemon/`（單一 crate，bin `agents-managerd`，子命令 `serve` / `hook`）、`web/`。

## 9. 非目標
xterm.js 串流、diff 檢視、transcript 回補、Codex notify chain、`toml_edit` 保註解、WS 推終端畫面、Project 刪除時關 workspace。

## 11. 遠端主機

### 11.1 目標
Project 可以在另一台機器：那台有自己的 herdr，agent 在那台的 pane 裡跑，daemon 仍在本機，UI 操作相同。herdr 的 `--remote` 只支援 TUI attach，所以用 **OpenSSH 轉發**：

```
本機 daemon ──(ssh -M master)──► 遠端 sshd
   │  -L <本機短路徑>.sock : ~/.config/herdr/sessions/<session>/herdr.sock   （herdr RPC / 事件）
   └─ ssh <host> '<sh 指令>'                                                 （放 hook 腳本、settings、讀 spool、列目錄）
```

沒有反向轉發：遠端 hook 不打 HTTP 回本機，狀態走 herdr 事件、內容走 spool 檔（§11.4）。

### 11.2 設定
```toml
[[hosts]]
name = "m4p"                       # [a-z][a-z0-9_-]{0,31}；"local" 保留給本機
ssh = "m4p@100.112.229.82"         # 可用 ssh_config 別名
ssh_port = 22
herdr_session = "agents-manager"   # 遠端 named session（絕不用遠端 default session）
remote_path = "/opt/homebrew/bin:$HOME/.local/bin"   # 非互動 ssh shell 缺的 PATH，前置

[[projects]]
host = "m4p"                       # 缺省 = 本機
path = "/Users/m4p/work/foo"
label = "foo@m4p"
```
- 只用使用者現有的 ssh key / agent / ssh_config，一律 `BatchMode=yes`，絕不互動輸入密碼。認證失敗 → host `disconnected`，UI 顯示錯誤字串。

### 11.3 HostManager
每個 host 一個 `HostConn`：
1. **ensure remote session**：遠端是 macOS 且 ssh 使用者就是 `/dev/console` 擁有者時，寫 `~/Library/LaunchAgents/dev.agents-manager.herdr-<session>.plist`
   （`herdr --session <session> server`、`KeepAlive`、`RunAtLoad`、`ProcessType Interactive`、PATH 含 `remote_path`）並 `launchctl bootstrap gui/<uid>`；已載入就沿用；
   原本有 nohup 起的 server 先 `server stop` 再交給 launchd。其他情況退回 `( trap '' HUP; herdr --session <session> server & )`。
   理由：非互動 ssh 讀不到登入 Keychain（errSecInteractionNotAllowed），在那底下起的 Claude Code 會「Not logged in」；GUI 網域的 LaunchAgent 在桌面工作階段裡，Keychain 已解鎖，
   而且當掉會被 launchd 拉起。
2. **master 連線**：`ssh -N -M -S <ctl> -o BatchMode=yes -o ExitOnForwardFailure=yes -o ServerAliveInterval=15 -o ServerAliveCountMax=3 -o StreamLocalBindUnlink=yes -L <local.sock>:<remote herdr.sock> <target>`。
   `<local.sock>`、`<ctl>` 放短路徑 `/tmp/agents-manager-<uid>/<host>.sock|.ctl`（macOS AF_UNIX 上限 104 bytes）。
3. `HerdrClient::new(<local.sock>)` 取得與本機相同的 client，`ping` 成功 → `connected`。
4. 每 10 秒 `ping`；失敗或 master 退出 → `disconnected`、指數退避（1s→30s）重建 → 成功後對該 host 對帳並重建事件訂閱。
5. daemon 退出時 `ssh -O exit`；遠端 herdr 與 agent 保持存活。
6. `App.hosts: HashMap<String, HostConn>`，`"local"` 為本機；一律 `app.herdr_for(project.host)`；pane watcher、fallback timer 以 `(host, pane_id)` 為鍵；對帳與全域訂閱逐 host。

### 11.4 遠端 hook：herdr 事件 + spool 檔

遠端沒有 `agents-managerd`，hook 是 daemon 寫過去的 POSIX sh 腳本。不用反向埠（`ssh -R`）：`ExitOnForwardFailure` 讓「遠端埠被佔」等於整台主機斷線，
而且預設埠 7788 遇到遠端也跑一份 agents-manager 必撞。daemon 已經有一條可靠通道（`-L` 的 herdr socket），所以走混合路徑：

| 走什麼 | 用什麼 | 為什麼 |
|---|---|---|
| 狀態（idle、native session id） | 遠端 hook 呼叫 `herdr pane report-agent` | 事件經 herdr socket 回到 daemon |
| 內容（完整事件 JSON） | 追加到遠端 `~/.config/agents-manager/bots/<bot_id>/hook-spool.jsonl` | herdr 事件不帶 hook payload，內容只能落地再讀 |

本機 bot 仍走 `agents-managerd hook <provider>` → HTTP（§4.4）。

#### 11.4.1 herdr 端的事實（0.8.2）
- `herdr pane report-agent <PANE_ID> --source <ID> --agent <LABEL> --state <idle|working|blocked|unknown> [--message] [--seq <N>] [--agent-session-id] [--agent-session-path]`。
- 狀態會變成 `pane.agent_status_changed`，data 只有 `{pane_id, workspace_id, agent, agent_status}`——**不帶** message/source/seq/session id，所以內容一律走 spool。
- `--seq` 是每個 `(pane, source)` 各自的單調計數，小於等於上一次的被丟掉 → 每個 bot 固定一個 source，seq 嚴格遞增。
- `report-agent-session` 不發任何事件，只當 best-effort；session id 從 spool payload 讀。
- 狀態上報與 herdr 的終端偵測並存；相同狀態不再發事件，所以「偵測先報 idle、hook 才寫 spool」是真實競態（§11.4.4 補）。

#### 11.4.2 遠端 `hook.sh`
路徑 `~/.config/agents-manager/bots/<bot_id>/hook.sh`，每次啟動 Run（與 reconcile 修 hook 時）覆寫。argv `hook.sh <provider> <bot_id> <token-slot>`：
第 3 個參數 daemon 填 `-`、腳本不讀——codex 的 notify argv 在 `ps` 對全機使用者可見，而 hook token 同時是本機 `/hook/*` 與 `/relay/announce` 的鑰匙。

| provider | payload 來源 | 上報狀態 | 寫 spool |
|---|---|---|---|
| `claude` | stdin（≤1 MiB，超過截斷標 `truncated`） | `SessionStart` → 只 `report-agent-session`；`Stop` 且 `stop_hook_active=false` → `--state idle` | 兩者都寫 |
| `codex` | argv 最後一個（JSON） | `agent-turn-complete` → `--state idle` | 寫 |
| `grok` | stdin | `session_start` → 只 `report-agent-session`；`stop` 且 `end_turn` 且 `stopHookActive=false` → idle；`shutdown` 不報 | 寫 |
| `statusline` | stdin | 不報 | 不進 spool（§11.4.5） |

- **腳本不做語意判斷**：只用最粗的字串比對決定要不要報 idle，其餘照寫 spool，分類只在 `hookrecv::classify`。遠端腳本沒有測試；漏報最多晚一點被掃到，錯分類會吃掉訊息。
- **先寫 spool，再 `report-agent`**（反過來 daemon 收到事件時 spool 還沒那行）。spool 行格式同 §4.4（`{bot_id, provider, payload, received_at, truncated}`），`O_APPEND`。
- `report-agent` 欄位：`$HERDR_PANE_ID`（沒有就跳過上報）；`--source agents-manager:<bot_id>`；`--agent <kind>`；`--state` 只送 `idle`（`working` 交給終端偵測，硬報會互蓋）；
  `--seq` 有 `python3` 用 `time.time_ns()`，否則 `date +%s`×1000 + `$DIR/hook-seq` 計數；`--agent-session-id`／`--agent-session-path` 有才帶；`--message` 不填。
- 找 herdr：`${AM_REAL_HERDR:-}` → `command -v herdr`；都沒有就只寫 spool、記 `hook.log`、exit 0（30 秒掃描會補）。`HERDR_SESSION` 有值時帶 `--session`。
- 契約同 §4.4：≤ 3 秒、永遠 exit 0、空 stdout（grok 的 Stop hook 會把 stdout 當 decision）。

#### 11.4.3 daemon 端：狀態事件 → 讀 spool → 重放
`events::handle_status` 在遠端 run 上多一步（per-bot 鎖內，與 HTTP hook 同一把）：
1. 照舊更新 `agent_status`、推 WS。
2. host ≠ local 且（`working → idle` 或 `→ blocked`）→ **drain**（`hookrecv::replay_spool_remote`）：一段 ssh sh 把 `hook-spool.jsonl` `mv` 成 `.replaying`
   （已存在就把新的接在後面）→ `cat` → `rm`；daemon 逐行解析走 §6.7。
3. drain 是 await 的（預算 4 秒），成功後才 `arm_fallback`——終端備援只在 hook 真的沒來時才贏；失敗或逾時照舊 arm，CAS 保證不雙寫。
4. 冪等：rename 是遠端原子操作；重放再靠 `(native_session_id, native_turn_id)` 去重。drain 全程持鎖。

#### 11.4.4 遲到、重複與遺失
- **重複事件**：同一 bot 的 drain 有 1 秒合併窗，窗內第二次觸發只記「還要再跑一次」。
- **事件先到、spool 後寫**：拿不到 → T+2 秒再 drain 一次（早於 5 秒的終端備援），仍沒有就讓備援接手。
- **事件整個遺失**：每台已連線 host 每 30 秒掃「有 in-flight Turn 或 spool 檔存在」的 bot 做 drain（一台一次 ssh，腳本內迴圈所有 bot 目錄）；host 重連與啟動對帳對每個 bot drain 一次（`replay_host`）。
- **遲到的 hook**：對應 Turn 已 `completed_fallback` → 依 §4.3 丟棄只 log。
- **bot 已刪除**：`process_locked` 擋 `deleted_at`；遠端 bot 目錄在刪除時 `rm -rf`。
- **host 斷線期間**：hook 照寫本機檔，重連後 `replay_host` 補進來。

#### 11.4.5 statusLine（額度）
claude 的 statusLine 每次重繪都呼叫、沒有回合語意，不進 spool，改**單槽檔**：
- `hook.sh statusline …` 把 stdin JSON 加 `"hook_event_name":"StatusLine"` 後**覆寫**遠端 `bots/<bot_id>/hook-status.json`，再 exec 使用者自己的 statusLine 命令（讀遠端 `~/.claude/settings.json`）。
- daemon 每次 drain 的同一段 ssh 順便 `cat` 並 `rm` 這個檔，當成 `provider=claude`、`StatusLine` 的 `HookBody` 走 `HookKind::StatusLine`（寫 `runs.status_line` / `status_json`，§14）。
  遠端額度最多晚 30 秒。

### 11.5 目錄選擇器
`GET /api/fs/dirs?host=<name>&path=` 對遠端跑一段 sh（`cd <path> && pwd && for d in */ .[!.]*/; …` 輸出 `名稱\t是否有 .git`），daemon 解析成與本機相同的 JSON
（`home` 取 `echo $HOME`，`~` 前綴展開）。隱藏目錄預設略過，`hidden=1` 才列。

### 11.6 API 與 UI
- `GET /api/state` 帶 `hosts: [{name, ssh, herdr_session, connected, error?}]`、`projects[].host`；`POST /api/hosts`、`DELETE /api/hosts/:name`（需無 project 使用）、`POST /api/hosts/:name/reconnect`；
  `POST /api/projects` 可帶 `host`。WS `daemon_status {herdr_connected, hosts}`、`host_changed`。細節見 `API.md`。
- UI：sidebar Project 標題顯示 host 徽章（本機不顯示）；新增 Project 表單有主機下拉，目錄選擇器跟著切換；主機管理表單列出連線狀態與重連；host 斷線時其 bot 燈號灰。
- **shell 的鍵盤同步**（使用者 2026-09-16）：shell 面板的「鍵盤同步」開關打開後，終端本身收鍵盤，每一下按鍵原樣送進那個 pane（`…/shells/{pane}/keys`），
  貼上走 `…/text` 且 `enter:false`（不拆成鍵——換行會變成 Enter 直接執行）。輪詢從 1 秒加快到 0.25 秒，指令列讓位（disabled）。
  ⌘ 系列與 herdr 不收的鍵（Delete／Home／End／PgUp）回 `null` 留給瀏覽器，使用者不會被關在框裡。開關狀態每個 pane 各記一份（localStorage）。
  送出一律走同一個佇列（`keyQueue`）：同一時間只有一個請求在路上，否則抵達順序不保證，打 `ls` 可能變成 `sl`。

### 11.7 不做
密碼／互動認證、跳板（交給 ssh_config 的 ProxyJump）、遠端 transcript 回補、多 daemon。

### 11.8 開發測試
`scripts/dev-sshd.sh` 以使用者權限起 127.0.0.1:2222 的 sshd，`host = "loop"`（session `am-loop`）。

## 12. grok 支援

`kind = "grok"`：xAI grok CLI（`~/.grok/bin/grok`）。herdr 內建 `grok` agent manifest，`agent.start {kind: "grok"}` 直接可用，狀態偵測靠 OSC title / OSC 9;4 progress / 畫面規則（附錄 F）。

### 12.1 啟動參數
| 項目 | 注入 |
|---|---|
| `auto_approve` | `--always-approve`（= `--permission-mode bypassPermissions`） |
| `model` | `-m <model>` |
| `effort` | `--reasoning-effort <level>` |
| hooks | 無 argv（§12.2） |

argv 順序：daemon 旗標 → model → identity.args → bot.args。新目錄可能出現 trust 對話框 → `blocked`，UI 送鍵處理。遙測 banner 不阻塞輸入。

### 12.2 hook 注入：全域 hooks 檔 + env 分派
grok TUI 沒有每次啟動注入 hook 的旗標（`--settings`/`--hooks`/`--plugin-dir` 都不收）；hook 只能來自 `<GROK_HOME>/hooks/*.json`（全域、永遠信任）、
`<project>/.grok/hooks/`（需 trust、會污染 repo）、`config.toml` 或 plugin。所以用**全域 hooks 檔 + pane env 分派**：

1. 啟動 grok bot 時（不論 `inject_hooks`；內容固定，變更才覆寫）寫：
   - `~/.config/agents-manager/grok-hook.sh`：
     ```sh
     #!/bin/sh
     [ -n "$AM_BOT_ID" ] && [ -n "$AM_HOOK_TOKEN" ] || exit 0
     exec '<abs agents-managerd>' hook grok --bot "$AM_BOT_ID" --token "$AM_HOOK_TOKEN" --port "${AM_PORT:-7788}"
     ```
     遠端版 `exec "$HOME/.config/agents-manager/bots/$AM_BOT_ID/hook.sh" grok "$AM_BOT_ID" "$AM_HOOK_TOKEN"`。
   - `<GROK_HOME>/hooks/agents-manager.json`：`SessionStart` 與 `Stop` 各一個 command hook 指向分派腳本（`timeout: 5`）。`GROK_HOME` 取自 identity.env ∪ bot.env，缺省 `~/.grok`。
2. `inject_hooks = false` 時 pane 不給 `AM_HOOK_TOKEN`，分派腳本立即 exit 0（走終端備援）。
3. 使用者自己開的 grok（無 `AM_BOT_ID`）只多一次 `sh` 啟動。Stop hook 的 stdout 必須空（JSON 會被當 decision），§4.4 已保證。
4. 刪 bot 不移除全域 hooks 檔（沒有 grok bot 時是無害 no-op）。

### 12.3 事件分類與回覆擷取
- `hookrecv::classify("grok")` 看 `hookEventName`（或 `hook_event_name`）：
  - `session_start` → Identity（`sessionId`）。grok 的 SessionStart **延遲到第一次 prompt 才觸發**。
  - `stop` 且 `reason = "end_turn"` 且 `stopHookActive = false` → TurnComplete（`sessionId`、`promptId`、`transcriptPath`、`lastAssistantMessage`）；`reason = "shutdown"` 忽略。
  - 其他忽略。
- 終端備援：grok 回覆是無標記的縮排純文字，右側帶 `h:mm AM|PM` 時戳與捲軸 `█`。`extract_reply` 回 `None`；`clean_screen` 另外去掉行尾 `█` 與時戳、`◆ …` 事件行、
  `Worked for …  stop [hooks: N]`、`<cwd>  15K / 500K` 標頭、`[stable]`、頁尾快捷鍵，以及整塊「Help improve Grok … Privacy Policy.」。回音字元與 Claude 同為 `❯ `。

### 12.4 身份隔離
`GROK_HOME`（預設 `~/.grok`）等同 Claude 的 `CLAUDE_CONFIG_DIR`：config、`auth.json`、`sessions/`、`hooks/` 都跟著走；hooks 檔寫到該 `GROK_HOME/hooks/`。

### 12.6 額度：`/usage` 探測
grok 沒有 usage 子命令或 RPC，數字只在 TUI 的 `/usage` 對話框裡，所以開**用完即丟**的 workspace 探測，跑在**專屬 herdr session `am-quota`**（需要時起、永不 attach）：
pane 寬度來自 attach 的 client，窄終端會把百分比截掉；沒有 client 的 session 用寬預設格線。也避免在使用者的 workspace 閃 pane。

流程：`workspace.create`（label `am-quota-grok`、cwd 家目錄）→ `agent.start` kind grok、名稱 `amquota<6碼>`（不在 DB，對帳不會當成 bot）→ `agent.wait` 再等 3 秒 →
`pane.send_text "/usage"`、0.8 秒後 Enter → 每 0.9 秒 `pane.read visible 120`，最多 25 秒 → `workspace.close`（在 `Drop` 裡）。
poller 啟動時 `sweep_stale()` 關掉 `am-quota` 與本機 session 裡 label 為 `am-quota-grok` 的殘留。

解析（`quota_grok.rs`）：標題 `<window> limit (<plan>)`，window 含 `week` → `seven_day`、含 `hour` → `five_hour`；百分比只認同列有 `█`/`░` 的（避開 Context usage 分頁）；
`Resets:` 沒年份，補當年、已過期超過一天就進位隔年，視為本機時區、輸出 RFC3339 UTC。頻率：啟動一次，之後每 30 秒（使用者指定）；`GET /api/quota?refresh=1` 也觸發。

## 13. 專案群組聊天

### 13.1 資料模型
- **一個 Project 就是一個群組**，成員 = 其下所有存活的 bot；不另設 conversation 型別。時間軸 = 成員所有訊息依 `message.id`（ULID）合併，每則附 `bot_id`、`bot_name`。
- 群組發言以 §6.3 `prompt()` 送給每個目標：**各建一個 Turn 與一則 user Message**，`client_request_id = <crid>:<bot_id>`。
- `messages.group_id`：同一次發言的 user 副本與「未送達」system 註記共用（= 該次 `client_request_id`）；回覆為 NULL。前端把同 `group_id` 的副本折成一則並列出目標。
- 送給 agent 的文字去掉 mention。

### 13.2 mention 解析（後端為準，前端只提示）
- `@all` = 專案內所有 bot（不分大小寫）。`@<name>` 比對暱稱，支援 unicode 暱稱與全形標點；尾隨標點切掉。
- `@` 須在開頭或非字元之後（`me@example.com` 不算）。沒有有效 mention → 400 `{error:"no_mention", message, bots}`。目標依專案內 bot 順序去重。

### 13.3 不可送的 bot（略過，不自動啟動）
沒有 active Run、非 `running`、`blocked`、已有 in-flight Turn、有 `delivery=unknown` → 略過，列在 `skipped:[{bot_id, bot_name, reason, detail}]`
（`reason ∈ not_running | blocked | in_flight | unknown_delivery | conflict | not_found | bad_request | upstream`），並在該 bot 對話寫一則同 `group_id` 的 system Message
（「群組訊息未送達 X：…（不會自動啟動）」）；同 crid 重送不重寫。**絕不自動啟動。**

API：`GET /api/projects/:id/messages`、`POST /api/projects/:id/chat`（`API.md`）。WS 沿用 `message_added`（含 `group_id`）與 `turn_updated`。

### 13.4 前端
- sidebar Project 標題可點進群組視圖；標題列顯示專案名、群組標籤、host 徽章與成員燈號列（點成員跳到單獨對話）。
- 時間軸：bot 回覆／system 訊息帶 bot 名徽章（依 kind 配色）；user 副本折疊顯示 `→ @a, @b`；每個仍在回覆的成員各一個 typing 指示。
- 輸入 `@` 彈出成員與 `all` 自動完成；沒有 mention 時送出鈕 disabled；列出收件者並標示將被略過的。**專案內至少一個 bot 可送就不鎖輸入框。**
- 未打開群組時 sidebar 顯示未讀計數（前端記憶體，打開即歸零）。**只算群組回覆**（2026-09-15）：回的是群組訊息的那一回合（該 bot 對話裡有同 `turn_id`、帶 `group_id` 的 user 訊息）完成才 +1；成員各自的單獨對話不算。群組來源以持久化的 `messages.group_id` 確認；未載入原訊息時，`client_request_id` 的 `<crid>:<bot_id>` 僅作查詢候選，再向訊息 API 查該 bot、該回合的 user 訊息，不以命名或歷史頁數當證據。群組計數保存在 localStorage，v3 遷移先保存清空結果才寫完成標記，bot 未讀保留；包含 v2 已遷移的瀏覽器會再清一次群組計數。

## 14. 每台主機各自的額度

**額度屬於它被讀到的那台主機**。標題列一次只顯示一台：預設本機，點進 ssh 主機上的 bot 或專案時換成那台（使用者決定：切換，不並列）。

### 14.1 Key 與資料形狀
- map key：本機裸的 `claude` / `claude:cc1` / `codex` / `grok`；遠端加前綴 `m4p/claude`、`m4p/claude:cc1`…（與 `GET /api/models` 的 `host/kind` 快取同形；host 名不含 `/`、`:`，拆得回來）。
- 每筆多 `host` 欄位。`quota::set(app, host, base_key, q)` 統一蓋章：呼叫端只給裸 key，沒有路徑能把遠端讀數存進本機那列。
- 刪主機連同它的額度列；`GET /api/quota` 丟掉不屬於現存主機的 `<host>/…` key。

### 14.2 三個來源都跟著主機走
- **codex**：`codex_rpc(app, host, "account/rateLimits/read")`，遠端走 `ssh_exec_path`。另外每 60 秒讀 running codex pane 底下的狀態列（`5h 90% left · weekly 48% left`，`source=codex-statusline`；app-server 每 5 分鐘才問一次、而且落後），寫同一把 key、後到覆蓋——狀態列是 CLI 當下拿來擋人的依據。同帳號有多顆 pane 時先讀最近有回合的那顆，讀不到狀態列（壓縮對話中、捲動中）就換下一顆（2026-09-15 使用者：pane 寫 93% left、header 還是 100）；它沒有重置時間，`resets_at`／`reset_credits`／`limit_hit` 沿用前一份。
- **claude statusLine**：hook 進來時查 `bot_host(bot_id)` 寫進那台的列（遠端經 §11.4.5 的單槽檔）。有 bot 在對話的帳號就有即時數字。
- **claude `/usage` 探測**：在用完即丟的 pane 跑一行 `claude auth status --json` 接 `claude -p "/usage"`，輸出以 `AM_AUTH_BEGIN` / `AM_AUTH_END` / `AM_USAGE_DONE=` 標記包起來，
  `pane.read recent_unwrapped` 等到最後標記（逾時 40 秒）。`-p` 印純文字、不會有 TUI 對話框或信任視窗；同一次探測順便拿到該身份的登入狀態、`account`、`plan`
  （claude 身份不經 ssh 探登入：非登入 ssh 讀不到 Keychain）。沒登入的身份 park 30 分鐘，其他失敗 5 分鐘。
  `/usage` 先跑 `--output-format stream-json --verbose` 並 `grep -m1 usage_report`：claude 2.1.273 起那一行帶結構化的 `usage_report.rate_limits.limits[]`
  （`kind` = `session`／`weekly_all`／`weekly_scoped`＋`scope.model.display_name`、`percent`、ISO `resets_at`、`severity`），分桶一律看 `kind` 不看顯示字串，重置時間直接用 ISO。
  `grep` 沒抓到（舊 CLI 不認這個旗標或還沒有這個欄位）才跑純文字版，交給既有的文字解析（`parse_claude_usage`）。
- **grok `/usage` 探測**：§12.6 的 TUI 流程。
- 兩者本機開在專屬 `am-quota` session；遠端借 **daemon 在那台的 named session**（遠端只有一條轉發 socket，再開 session 要多一條轉發）。
  label 是 `am-quota-claude*` / `am-quota-grok`、agent 名 `amquota<6碼>`（不在 DB）；`sweep_stale()` 掃本機 `am-quota` 與每台已連線主機的 session。
  cwd 與 identity env 的 `~` 用那台主機的 `$HOME`（`HostConn::home()`）。

### 14.3 輪詢
codex 5 分、claude 60 秒、grok 30 秒；每輪對 `local` + 每台已連線遠端各跑一次（使用者決定：持續輪詢，不只在檢視時），同輪各主機併發（`JoinSet`，一次探測數十秒）。
斷線主機跳過。`probe_lock` per host；同一台的多個 identity 一個一個探（共用 pane）。`GET /api/quota?refresh=1` 依序。

- claude 跳過條件：該列在 60 秒內剛被 statusLine 更新過**且**該身份的 `logged_in` 已知（登入答案搭同一次探測回來，還沒答案的仍值得探一次）。
- grok 同理：沒登入不探；探測失敗後該主機停 5 分鐘再試。`refresh=1` 不受節流，永遠真的探。

### 14.4 API / WS
- `GET /api/quota?refresh=1[&host=<name>]`：給 `host` 只重讀那台（不存在 404）。回應永遠是完整 map（每台主機三個基本 kind 都有 key，沒資料 `null`）。
- WS `quota_updated` 的 `kind` 是完整 key，另帶 `host`。

### 14.5 UI
- 額度條吃一個 `host`：bot 對話用該 bot 專案的 host、群組用 Project 的，都沒選是本機。
- 遠端時條最左掛主機名牌（`.quota-host`），本機不掛；tooltip 以主機名開頭，popover 標題「本機額度」/「m4p 的額度」。
- 側欄 bot 的 critical 警告讀該 bot 所在主機的列。
- 兩個 daemon 管同一台遠端會搶同一條 ssh master（`hosts::short_dir()` 只用 uid 命名），測試 daemon 要先停掉另一個。

## 15. herdr 這一側的記憶體

### 15.1 量什麼
整棵 **herdr 進程樹**（herdr + 底下的 pane 與 agent CLI）。樹根 = 執行檔名是 `herdr` 的 process（argv 裡剛好有這字的不算），herdr 底下再開 herdr 只算一次。
每台主機每 15 秒 `ps -Awwo pid=,ppid=,rss=,args=`（遠端走 ssh master），變化超過 1 MiB 才推 `mem_updated`。量不到用 `error` 回報，不從清單消失。端點 `GET /api/mem`。

### 15.1a 這台機器還剩多少
同一次取樣多帶 `hosts[].machine: {total_bytes, available_bytes}`（一次 shell 往返）：
- Linux：`/proc/meminfo` 的 `MemTotal` / `MemAvailable`。
- macOS：`sysctl -n hw.memsize`；可用 = `vm_stat` 的 free + inactive + speculative + purgeable × page size（inactive／purgeable 是可回收快取）。

`available` 不是 `total − 我們用掉的`（還有瀏覽器與系統）。認不出輸出就回 `null`，UI 只顯示已用量——不猜。
UI：左上格「已用 · 剩 N」，剩餘 < 15% 轉警示色；明細第一行「這台機器 剩 N / 共 M（已用 …，其中 herdr 樹 …）」。

### 15.1b 每個專案佔多少（2026-09-15）

側欄（手機是選單抽屜）每個專案標題旁標「N pane · RAM」。來源是 `GET /api/mem` 的 `projects`：同一次 15 秒取樣裡，對每台量得到的主機多跑一趟帶環境變數的 `ps`，把 herdr 樹裡帶 `AM_BOT_ID` 的程序歸給那顆 bot 的專案——每個程序只算自己的 RSS（child 程序繼承變數、自己算），pane 數以 socket＋pane id 去重。沒在跑的專案不畫；量不到就不畫，不畫成 0。使用者自己開的 shell pane 沒有 `AM_BOT_ID`，不算進任何專案。

### 15.2 展開看程序 / 砍程序
**owner 判定讀 process 環境變數，不讀我們的帳本**：daemon 起 bot 注入 `AM_BOT_ID`，herdr 對每個 pane 注入 `HERDR_PANE_ID`，子孫繼承——連 daemon 開機前就在跑的也判得對。
macOS `ps -Ewwo pid=,args=`，Linux `/proc/<pid>/environ`。

| owner | 條件 | UI |
|---|---|---|
| `bot` | 有 `AM_BOT_ID`（bot 已刪也算） | 「停止 bot」 |
| `pane` | 只有 `HERDR_PANE_ID` | 「結束」→ 再按「強制」 |
| `herdr` | 執行檔就是 herdr | 不列、不可砍 |
| `unknown` | 都讀不到 | 同 `pane` |

只列 `claude`/`codex`/`grok`/`node`/`bash`/`zsh`/`sh`/`fish` 且 `subtree_bytes ≥ 8 MiB` 的，其餘併進父程序；依 `subtree_bytes` 排序（「砍這個能省多少」）。
owner 格可點開唯讀的 pane 畫面（`GET /api/mem/processes/pane`，`pane.read visible`，每 2 秒重讀，不給打字）；bot 列不給看（有自己的終端分頁）。

砍之前**一定重新取樣**再判定，不信前端送來的那列（pid 會回收）：不在樹裡 400、`herdr` 本身 400、`owner=bot` 409（走 `POST /bots/{id}/stop` 才會記錄）。砍完立刻取樣推 `mem_updated`。

## 16. 從 shell 認出來的身份 cc0～cc6

多帳號的人已把帳號寫在 shell 裡（`alias cc1='CLAUDE_CONFIG_DIR=$HOME/.claude-cc1 claude …'`），daemon 直接讀，不必手寫 `[[identities]]`。

### 16.1 怎麼讀
跟在工具偵測後面，同一個腳本、同一次 ssh：

```sh
al=$( "${SHELL:-/bin/sh}" -lic 'alias' 2>/dev/null )   # 登入 shell：zshrc / bashrc / 被 source 的都算
[ -n "$al" ] || al=$(cat "$HOME/.zshrc" 2>/dev/null)    # $SHELL 不是互動 shell 時的退路
printf '%s\n' "$al" | grep -E "(^|[[:space:]])(alias[[:space:]]+)?cc[0-6]="
```

解析（`tools::parse_shell_identities`）：名字正好 `cc0`…`cc6`；命令裡真的跑 `claude`（或 `…/claude`）；只取**開頭**的 `CLAUDE_CONFIG_DIR=`（前面只能是其他 `VAR=value`），
值到第一個未引用空白、剝引號；**旗標一律不取**（使用者決定：授權旗標由 `auto_approve` 決定，兩邊注入會打架）；同名取最後一個；沒有 `CLAUDE_CONFIG_DIR` 的（典型 cc0）是 env 為空的預設帳號。

### 16.2 每台主機各一份
**不寫回 `config.toml`**（使用者決定）：同名 `cc1` 在不同主機是不同帳號。
- `hosts[].shell_identities`：那台讀到的 `ccN`，每次偵測重讀。
- `hosts[].identities.<name>`：登入狀態 + `source`（`config`/`shell`）+ `config_dir`（用那台的 `$HOME` 展開）。無法判定時 `logged_in: null` 並帶 `reason`（CLI 缺失、指令失敗、解析失敗），UI 顯示「未知」。
  - 登入探測（`tools::login_probe_args`）：codex `login status`、grok `models`、**本機 claude `auth status --json`**（帶該身分的 `CLAUDE_CONFIG_DIR`，每次完整偵測都問）。遠端 claude 不問——非登入 ssh 讀不到 Keychain 會謊報 `loggedIn:false`——改由 pane 的 `/usage` 探測回填。
  - **這一輪問不到就沿用上一輪**（`tools::carry_over`）：整張表每次重建，不沿用的話每次重探都把 claude 身分打回「未知」，看起來像帳號自己登出（2026-09-16 使用者）。真的回 `loggedIn:false` 照樣覆蓋。
- 合併只有一條 `tools::identities_for_host()`：**config 的 `[[identities]]` 先，同名 shell 身份讓位**——但只限**屬於那一台**的那幾筆。
- **`[[identities]]` 有 host 維度**（AGM 裁示 2026-09-16）：鍵是 `(host, name)`；`host` 省略＝**只適用本機**。
  以前沒有這一欄，一個全域設定會靜默遮蔽掉每一台機器上同名的 `ccN`：使用者寫一筆 `cc1` 想固定本機的帳號，m4p 上所有選 `cc1` 的 bot 就被注入一個那台根本不存在的設定目錄，
  claude 以未登入狀態開一個空的設定目錄，額度探測也去問那個空目錄——UI 上兩者名字一模一樣，沒有任何地方會提示它們不是同一個帳號（review 2026-09-16）。
  **現行 config.toml（不寫 host）在本機的行為一個字都沒變**，有測試釘住；變的只是它不再外溢到遠端。
  額度 key：遠端 bot 若因此改由那台自己的 shell 身分解析，key 可能從裸 `claude` 變成 `claude:<name>`（或相反）。`app.quotas` 只在記憶體、不落 DB，所以下一輪探測就收斂，沒有東西要搬。
  `POST /identities` 可帶 `host`（未知主機 409）；`DELETE /identities/{name}?host=` 刪的是那一台的那一筆，「還有 bot 在用」也只看同一台的 bot。

`identities_for_host(host)` 用在：啟動 bot 的 env／args、建立與 PATCH bot 及開團的身份驗證（在該 bot／專案的 host 上查）、claude 額度探測 targets、UI 身份選項與面板。
`[[identities]]` 仍可手寫、可從 UI 新增刪除；shell 認來的唯讀（要改去改那台的 alias）。

### 16.3a 主機層一鍵登入
`POST /api/hosts/{name}/identities/{identity}/login`：在該主機 manager session 開臨時 host-shell pane，identity env 以該主機 `$HOME` 展開後執行 `claude /login`、`codex login` 或 `grok login`。
pane 的終端快照是 UI 顯示 device code / URL 的唯一通道；這些內容不進 daemon log 或 WS 事件。CLI 結束（成功或失敗）後重新探測該身份再關 pane；建立或登入失敗走同一條清理路徑。

`POST …/logout` 走同一條路（同一個臨時 pane、同一組 env 前綴），指令換成 `claude /logout`、`codex logout` 或 `grok logout`。
環境前綴與登入共用同一段程式：少帶 `CLAUDE_CONFIG_DIR` 就會登出別的帳號。清掉的是那個身份設定目錄裡的憑證——執行中的 bot 不受影響，下次啟動才會停在登入畫面，所以 UI 先問一次並說明有幾顆 bot 綁著它。

### 16.3b 停用一個身份（使用者 2026-09-16）
`PUT /api/identities/{name}/disabled {kind, disabled, host?}`（daemon 的 `identity_prefs`，不是瀏覽器 localStorage）。
停用是**挑不挑得到**的問題，不是能不能跑：群組任務挑身分、Bot 設定的身份選單、**標題列的額度條**、快速新增 Bot 的清單、側欄的身分計數都不再出現它（使用者 2026-09-16：「停用就別顯示在 header 及任何地方」）；唯一還看得到它的是環境設定那一頁自己，不然沒有地方把它按回來。
已經綁著它的 bot 照跑，那顆 bot 的設定裡仍看得到自己選的那一個（否則設定看起來會像空的）。
額度也不再探測它（`quota_claude::refresh_claude` 跳過），**除非它還有 run 在跑**——停用是「別再挑它」，不是把正在用的額度弄瞎。
主機上的 alias 不動、登入狀態不動，隨時可以按「啟用」放回來。偵測到但不打算用的 `ccN`（例如只是 zshrc 裡留著）就用這個標掉。

### 16.5 alias 定期重讀
每 60 秒對每台可達主機只跑 alias 那半段 probe（一個登入 shell，不碰 CLI），與快取的 `shell_identities` 比對；**名單或 `CLAUDE_CONFIG_DIR` 變了**才做完整偵測（含登入探測）並推 `host_changed`。
還沒做過第一次偵測的主機不在此列。

### 16.6 收編的子 agent 身份怎麼判定
子 agent 的 pane 是母 bot 開的（`herdr pane split --env CLAUDE_CONFIG_DIR=…` 就能換帳號），收編時抄母 bot 的 identity 會把額度記到錯帳號。herdr 的 `pane.process_info` 不回 env，所以跟作業系統要：

1. 補 model/effort 的同一次 `pane.process_info` 挑出 CLI 行程的 `pid`。
2. `ps eww -p <pid>`（遠端走 `ssh_exec`）取 `CLAUDE_CONFIG_DIR` / `CODEX_HOME` / `GROK_HOME`（依 kind）。
3. 比對 `identities_for_host(host)`（config 優先）；比對前用那台的 `$HOME` 展開 `~`/`$HOME`，正規化結尾斜線與 macOS `/private` 前綴。對得上寫回 `bots.identity`。

- **只動 `managed_by='child'`**（其他 bot 的 identity 是使用者設定、會投影回 config.toml；SQL `WHERE` 也帶著）。
- model/effort 只補不改（argv 看不到之後打的 `/model`）；identity 補也改（那是收編時抄錯的值）。
- **預設帳號也認得**：沒有變數、或指向 CLI 預設目錄（`~/.claude`、`~/.codex`）= 空 env 的身份（cc0），多個時 config 優先、第一個贏。
- **永遠不清成 NULL**：讀不到、沒人認領、該 kind 沒空 env 身份 → 維持現狀。
- **一個 pane 只問一次作業系統**（重連重播一串 `pane.agent_detected`，每個小孩每次一個 ssh 會變風暴）；讀不到或那台還沒偵測出同 kind 身份不算問過。

## 17. claude 的 `--effort`

claude 有 `--effort <low|medium|high|xhigh|max>`：
- `config::efforts_for_kind("claude")` = 這五級；不合法值 400（例如 codex 的 `none` 或 TUI 才有的 `ultracode`）。
- 啟動注入 `--effort <level>`；argv 順序：daemon 旗標 → persona → model → effort → identity.args → bot.args。
- `GET /api/models` 的 claude 靜態清單每個 alias 帶同一組 `efforts`（claude 沒有 per-model 清單，UI 不標「依 <模型>」）。
- **可當場套用**：`/effort <level>` 帶參數直接生效；不帶參數才是拉桿。`PATCH /bots/:id {effort}` 走 `apply_live_setting`，回 `needs_restart: false`（只改這欄、run 在跑且不忙、不是清成 CLI 預設）。
- 副作用：claude 會順手存成該帳號新 session 的預設（`saved as your default for new sessions`）。拉桿的「只套用這次」要靠方向鍵定位、送不出確定值，所以用帶參數形式。

### 17.1 「預設」提示從哪來
claude 的預設強度來自**帳號的 `settings.json`**：

```json
{ "effortLevel": "high", "modelSettings": { "claude-opus-5": { "effortLevel": "low" } } }
```

- `effortLevel` 全域；`modelSettings.<真實 model id>.effortLevel` per-model 覆寫。真實 id 不是啟動用的 alias（`--model opus` 跑的是 `claude-opus-5`），
  所以用**子字串**比對 key 是否含 `opus`/`sonnet`/`haiku`/`fable`；不吻合就沒有提示，不猜。
- 兩者都沒有、讀不到、非法 JSON → claude 內建預設 **`high`**（官方文件：除 Opus 4.7 外所有模型預設 high；乾淨帳號真機確認 sonnet 與 haiku 皆 high）。
- `GET /api/models?kind=claude&host=&identity=` 的 `identity` 決定讀哪個 `CLAUDE_CONFIG_DIR/settings.json`（`identities_for_host`）；不指定或不存在 → 預設帳號 `~/.claude/settings.json`。
  本機讀檔、遠端 ssh `cat`。快取 key `{host}/{kind}/{identity}`，10 分鐘 TTL。
- UI tooltip 講來源：「不帶 --effort（帳號目前設定 高）」，不寫「模型預設」。
- `--effort bogus` 只印警告並用預設，不會讓 run 掛掉。

## 18. 總管（AGM）運維規範

AGM 的運維職責以本節為準，不靠任何 bot 的記憶。persona 是同一份規則的執行期投影（§18.11），launchd 腳本是實作；不一致時**以實際腳本行為為準**，再把本節改對。

### 18.1 開發用 dev server（5173）

- 位址 `http://<本機>:5173`，`--strictPort`（使用者手機書籤寫死，搶不到就失敗，不換 port）。
- **來源是只跟 origin/main 的乾淨 worktree** `/Users/m4p/project/agents-manager-main`（detached HEAD）。看門狗每輪 `git fetch && git reset --hard origin/main`（那棵樹沒有任何人的 WIP）；
  `web/bun.lock` 變了才 `bun install` 並重啟 vite，原始碼變動靠 HMR。**5173 看到的 = 已合併進 origin/main 的事實。** 共用工作樹不再被 5173 使用（它永遠有別人的 WIP、pull 不了）。
- **bot 驗自己未提交的改動用自己的 port**（例如 5188、`VITE_MOCK=1`），自己起自己收。5173 是使用者的視窗。其他 port 不歸看門狗管。
- **runtime 是 node 不是 bun**：`node web/node_modules/vite/bin/vite.js`。bun 的 upgrade socket 沒有 `destroySoon`，daemon 一重啟代理斷線 vite 就 crash。bun 只用來 build 與裝套件。
- **必綁 `--host 0.0.0.0`**（手機／LAN／Tailscale）；`vite.config.ts` 也設 `server.host: true`，手動起的也對外。代理把 `Origin` 改寫成 daemon 位址。
  代價：同網段裝置都能透過 5173 的 `/api` 代理打到 7788。
- **看門狗** launchd `com.agm.dev-server`：`StartInterval 60`（使用者指示；推上去後最多一分鐘可見）、`RunAtLoad`，跑 `bun run supervisor/AGM/bin/dev-server-kick.ts`，
  plist 的 `PATH` 要含 `/opt/homebrew/bin` 與 `~/.local/bin`（launchd 不給登入 shell 的 PATH）。行為：
  1. **健康 = 對外可達**：`lsof -nP -iTCP:5173 -sTCP:LISTEN -Fpn`（要用 `-F` 機器格式）綁 `*`／`0.0.0.0` 且 curl 127.0.0.1 有回應 → exit 0、不寫 log。只綁 loopback 的是**錯誤實例**。
  2. 錯誤實例：vite 且 `ppid=1`（孤兒）→ kill、等 port 放開（≤ 5 秒）再拉起；vite 但父程序活著 → 只記錄「需人工處理」；非 vite → 只記錄 pid 與 command。
  3. 找不到 node 或 `vite.js` → 記 log 跳過，**不拿 bun 代跑**。
  4. 沒人聽 → `nohup node vite.js --host 0.0.0.0 --port 5173 --strictPort`，≤ 15 秒複驗；失敗交下一輪，不在腳本內重試。
  log：`supervisor/AGM/dev-server.log`、`dev-server.launchd.log`。
- **驗證**（改規則或腳本後）：kill vite → 跑一次 kick → `curl 127.0.0.1:5173/`、`<LAN IP>:5173/`、`<LAN IP>:5173/api/session` 都 200。LAN IP 用 `route -n get default` 的介面問，不寫死 `en0`。

### 18.2 正式 daemon 的定義與例行更新

- **正式 daemon** = `target/release/agents-managerd serve`，`127.0.0.1:7788`，`web/dist` 內嵌。前端改動要 `bun run build` 再 `cargo build --release -p agents-managerd` 才進 7788。
- **例行檢查** launchd `com.agm.daemon-update`（每小時整點）跑 `daemon-update-kick.sh`：`git fetch` → 無程式碼差異跳過 → 同 commit 已派過 skip（`daemon-update.last`）→
  建置 child 不存在寫 `build child missing` 並 exit 0（不改派）→ 上一筆 `agm-daemon-update-*` 派工未結案 skip → 還有 bot `working` 就 defer →
  派 `daemon-update-task.md`（`--request-id agm-daemon-update-<sha>`）。
- **可動手的判準是沒有 bot 在 `working`**（看 `agm --compact state` 的 `run.agent_status`，排除建置 child 與 AGM 兩個角色），不是 `health.busy ≤ 1`：busy 含 `blocked`，
  而 blocked 可能等使用者好幾小時，重啟也不會打斷它。
- **誰重建**：有 bot 申請（帶已 push 的 commit）→ 核准後由申請者自己建；例行更新 → AGM 建置 child `agm-pxf2pv-build`（cc0/opus/low）。絕不派給使用者的專案 bot。
- **docs-only 不重啟**：建置驗證通過後把 origin/main short sha 寫進 `supervisor/AGM/daemon-update.built`（回滾不寫）。kick 用
  `git diff --quiet <built> origin/main -- daemon web Cargo.toml Cargo.lock` 判斷，沒差異寫 `docs-only since <built>, skip` 並推進 `.last`；檔案不存在或 sha 不在 repo 才退回 mtime 比較。
  執行端接到任務也先 diff，零程式碼差異只驗證並回報「無需上線」。申請重建前先查 `.built`，避免重複申請。
- **重建重啟固定條件**（全中才動手，否則回報阻塞）：
  1. 在**乾淨 HEAD worktree** 建 web 與 daemon（共用樹的 WIP 不得編進 release，不得 stash/reset）。
  2. 整樹 `cargo test -p agents-managerd` 全過，`bunx tsc --noEmit -p tsconfig.app.json` 通過（不帶 `-p` 是假綠燈）。
  3. 沒有別的 bot 在 `working`，最多等 30 分鐘，超過回報延後。**換 binary 前一刻再查一次**；更好的是用 §18.10 的租約持有窗口。
  4. 備份舊 binary 為 `target/release/agents-managerd.bak`。
  5. 重啟後 30 秒內驗 `/api/session` 與 `agm health`。daemon 起來即自動釋放 `restart` 租約，被 hold 的交辦馬上派送——**重啟後無等待期**（§18.10）。
  6. 60 秒內確認 `agm supervisor` 不是 stopped、running 名單沒少、沒有 bot 被無故關 pane。任一項不對用 `.bak` 回滾並回報。
  期間不要同時觸發 claude 更新批次重啟。
- 動 migration 的版本：上線前對正式 DB 的副本跑一次 migrate，重建申請附 DB 備份步驟。

### 18.3 喚醒 AGM 的節流
巡檢：`[supervisor] notify_interval_secs`（預設 600），規則見 §5；只有 `wake=1` 的事件會開一次喚醒，送前合併重複（§18.15）。協調者：短窗批次（§18.15）。
API 端狀態機見 `API.md` 的 `GET /api/supervisor/inbox`。

### 18.4 瀏覽器殭屍清理

launchd `com.agm.browser-gc` 跑 `bin/browser-gc-kick.sh`，`StartInterval` 依使用者 Claude 方案（方案變了改 plist 與這張表）：

| 方案 | StartInterval |
| --- | --- |
| Pro | 21600（6 小時） |
| Max 5x | 3600（1 小時） |
| Max 20x | 1800（30 分鐘，**目前**） |

每輪確認 `agm-pxf2pv-browser-gc` 在跑（沒跑就 `agm bot start`），派 `browser-gc-task.md`（request id `agm-browser-gc-<YYYYmmdd-HHMM>`）。規則：

- ego lite：`listTaskSpaces()`，`ownership=agent` 且無進行中 assignment、2 小時無活動才 `completeTaskSpace(id, {keep:false})`；**`ownership=user` 或 `agentDelegatedToUser` 一律不動**。
- **ego lite 的「ChatGPT 決策顧問」task space 與它的分頁一律不動**，不論 `ownership=agent`、閒置多久（使用者 2026-09-15；用途見 `docs/CHATGPT-CONSULT.md`）。
- OB 以 AG Man project ID 對應獨立固定對話；共用 Sonnet-low worker，每筆乾淨 context，原任務 bot 判讀結果。SQLite 持久佇列與 URL 唯一鍵隔離專案；Sonnet 額度不足留 waiting_quota、不回 Fable，送達未知不重送。完整 CLI／部署／對帳契約見 `docs/CHATGPT-CONSULT.md`。
  worker 崩潰後的 `running` 只在取得 worker lock 時恢復為 `unknown`，不自動重送；每次 claim 的識別值限制孤兒操作員回寫與送出，原回答由 request marker 及完成控制項確認，不依賴歷史 DOM 數量。
  CLI 卡死重開 ego lite 時回報受影響的 OB 請求；正常後續請求按 OB 資料庫回原 URL，`unknown` 先 collect／對帳，不自動重送。舊 JSON 只供明確 link，不能再當寫入索引。
- Chrome：只動 Claude in Chrome 的 MCP tab group。
- **bot 的 headless Chrome**（`--headless=new --remote-debugging-port=93xx --user-data-dir=/tmp/am-cdp-*｜/tmp/am-codex-*-profile｜/tmp/am-ui-rc`；使用者的 Chrome 沒有 `--headless`）：
  - 孤兒（`ppid=1`）+ debug port 無 ESTABLISHED + 活超過 2 分鐘 → TERM、3 秒後 KILL，刪 `/tmp/am-*` profile。三條缺一不可（`nohup` 起的在用實例 ppid 也是 1，但一定有 CDP 連線）。
  - 沒有 Chrome 在用、一小時沒動過的 profile 目錄 → 刪（一個 100～230 MB）。
  - 父程序活著但 bot 已 idle／run 結束且 Chrome 活超過 30 分鐘 → 收掉並記是誰。
  - 父程序活著且 bot `working`／`blocked` → 保留。
  回報一行：`headless Chrome：收掉 N（profile）／保留 M（bot）`。macOS `ps` 沒有 `etimes`，自己把 `[[D-]HH:]MM:SS` 換成秒。
- **所有 bot 的義務**：CDP／headless Chrome 用完自己關（`Browser.close` 或 kill 自己的 pid），profile 用完即刪。browser-gc 是安全網。
- CLI 超過 30 秒無回應：用 `ps` 回報 renderer 的 pid／記憶體／存活時間，**不直接 kill ego lite 主程序**；確認 CLI 無回應才走 quit → `pkill -f '/Applications/ego lite'` →
  對殘留 `--startup-ego-browser-service` `kill -9` → `open -a`。
- 記憶體不足導致指令被殺就回報並停止，不重試迴圈。

### 18.5 AGM 派 child 的模型預設
`cc0/opus/low`。不預設 `fable`、不預設 `high` 以上，任務明確需要才調高並記理由。巡檢自己的模型由 supervisor 控制器切換（`fable` 剩 < 5% 切 `opus`，30 分鐘冷卻內只自動切一次，不自動切回）；
協調者固定 `cc0/opus/high`，沒有自動切換（§18.15）。

### 18.5a Claude Code 換版就解析（使用者 2026-09-16）

launchd `com.agm.claude-release` 每 30 分鐘跑 `bin/claude-release-kick.sh`：比對 `~/.local/share/claude/versions` 最新的版本與 `claude-release.last`，
換版才派 `claude-release-task.md` 給 AGM 自己（`--review-by patrol`，request id `agm-claude-release-<版本>`），AGM 的回覆就是使用者看到的通知。規則：

- 第一次執行只記下目前版本，不為「本來就在的版本」派一次工；沒換版安靜退出（不寫 log）。
- 派工失敗不寫 `claude-release.last`，下一輪重派；同時只准一個執行者（`claude-release.lock`），殘留鎖交 AGM 檢查。
- 任務本身**唯讀**：比 `--help`、比 binary 裡的 `describe()` 欄位與 `CLAUDE*_` 環境變數、疑似有用的實測一次（拋棄式目錄 + `-p`），
  結論三到五行（是什麼、對應哪個痛點、要改哪個檔）。要改程式另外走派工與核准，不在這筆交辦裡動手。

### 18.6 persona 的副本
由 §18.11 規範：改人設走 `PUT /api/supervisor/persona`，不要手改 `config.toml` 或 `persona.md`。`ConfigStore` 寫入前比 mtime，磁碟變了會在同一把 mutex 內重讀再套用
（重新解析失敗才回錯），但 serde 全量回寫會洗掉註解與未知欄位。persona 改完不必重啟 daemon。

### 18.7 共用工作樹規範
`/Users/m4p/project/agents-manager` 是多個 bot 共用的工作樹，隨時有別人未提交的改動：

- **不改別人的 WIP，連新增幾行也不行**；不 `git stash`／`reset`／`checkout -- <file>`／`--autostash`，不對共用樹做 regex 批次取代。
- 認定 WIP 擁有者要有證據（該 bot 的訊息／assignment），不憑 pane 標題或 session 名稱猜。
- 髒到不能 `pull --rebase` 時，用暫存 index（`GIT_INDEX_FILE` + `read-tree origin/main` + `commit-tree`）或另開 `git worktree`；腳本用 `/usr/bin/git` 寫成 bash 檔（zsh 不拆 `$VAR`、rtk 會吞失敗）。
- 需要重疊範圍時交回 AGM 分配 ownership。

### 18.8 交辦的執行與驗收是兩件事

回合結束只結束那一輪（「還在等編譯，稍後回報」的回覆不代表做完）。`supervisor_assignments.status` 是生命週期：

| 階段 | 狀態 | 誰能推動 |
| --- | --- | --- |
| 執行中 | `queued` / `delivered` / `unknown` | daemon（派送、退避、對帳） |
| 等驗收 | `awaiting_review` | 只有 AGM 的決定 |
| 已決定 | `completed` / `failed` / `cancelled` / `superseded` | AGM |
| 還在等 | `blocked`、`quota_blocked` | AGM／daemon，**仍算未結案** |

- 回合原始事實各自留欄：`delivery`、`turn_status`（`completed` / `completed_fallback` / `failed` / `dispatch_failed` / `turn_missing` / `quota_exhausted` / `identity_switch`）、`evidence_complete`。
  終端備援不會因為「跑完了」就被驗收。派不出去的交辦也進 `awaiting_review`（`dispatch_failed`）。
- **送不進去的保險絲**（AGM 裁示 2026-09-16）：對方正在回合中會回 409，那是暫時的——但「暫時」要有盡頭。
  409 這條分支有自己的退避梯（15 秒起加倍，上限 `AM_DISPATCH_CONFLICT_BACKOFF_SECS`，預設 **900 秒**，要大於典型回合長度；其他分支的梯子不變），
  而且**有時間上限**：從建立起超過 `AM_DISPATCH_CONFLICT_GIVE_UP_MINS`（預設 30 分鐘）還送不進去，就把交辦標成 **`blocked`**（不是 `dispatch_failed`——工作沒失敗，是進不去），
  並推一則 `assignment_undeliverable` 進 inbox（AGM 看得到，不是只寫 log）。`blocked` 仍在 `OPEN_STATES` 裡，所以不會從未結案與 ownership 衝突裡消失。
  壞掉的環境變數（看不懂、0、負數）一律回預設；讀不懂 `created_at` 就繼續重試，不因為一個壞欄位把工作收起來。
  這條保險絲跟「派送真的排進佇列」是兩件事：後者（§4.4a 的 `queued` 生產者）上線之後，這條仍然有效——排進去也送不出來時一樣要看得見。
  2026-09-16 的實況：一張交辦對一顆回合 10～20 分鐘的 bot 重試 12 次、42 分鐘，狀態一直是 `queued`，最後由人手動取消——沒有任何地方會自己說「這件事沒送出去」。
- 驗收走 `POST /api/supervisor/assignments/{id}/review`，記 actor、來源、理由、證據（`supervisor_reviews`），同 decision 重送冪等。回合還在跑時只接受 `cancel`（不中止回合，之後的回覆不再記到這筆）；
  `block` 要等回合結束。
- 「要求續作」是新開一筆 `follow_up_of` 指回原本的交辦（原本變 `superseded`），用 `followup_request_id` 去重——不改寫已送出的文字。續作繼承 mission 連結。
- 未結案 = `queued`/`delivered`/`unknown`/`awaiting_review`/`blocked`/`quota_blocked`；open count、handoff、`/supervisor/state`、UI、例行更新判斷都吃這一組。
- 舊資料：migration 只把已關的舊 row 標 `legacy_closed=1`（UI 標「舊資料·未經驗收」），不重開不重派。

#### 18.8a 通知不是交辦
`supervisor_assignments.expects_review`（預設 1）。`expects_review=0`（API `kind:"notice"`、CLI `agm assign --notice`）：送達且回合 `completed`/`completed_fallback` → 直接 `completed`，
inbox `assignment_noticed`（`needs_review=false`）。送不出去或回合失敗的通知**仍**進 `awaiting_review`。`assignment_stalled` 跳過停在 `awaiting_review` 的通知；`quota_blocked` 不算卡住。

#### 18.8b 撞到用量上限是「等」，不是「失敗」
`quota.limit_hit` 記 CLI 印的上限橫幅（`You've hit your usage limit …`，可能帶 `try again at …`）。狀態 `quota_blocked`（未結案）：

- **派送前**（`controller::dispatch`）與**回合結束後**（`on_turn_done`）各查一次 `quota::limit_hit_for_bot`，撞到就停在 `quota_blocked`。
  回合結束那次的「回覆」是系統錯誤，記進 `error`，不寫 `result`、不算 `completed`。
- **`resume_at` 取橫幅與 app-server 讀數中最早且仍在未來的**（橫幅會舊，`five_hour.resets_at` 會延遲）。防線：橫幅時間只過去 ≤ 15 分鐘視為舊橫幅，改 5 分鐘後再問，不滾到隔天；
  算出等待 > 6 小時改 15 分鐘後重試；兩邊都沒有時間退回 +30 分鐘。
- **上限橫幅三條規則**：① 寫進該 bot 身份的 key（`quota_base(kind, identity)`，與 `limit_hit_for_bot` 的 `<kind>:<identity>` → 裸 kind 查法一致）；
  ② 只設 `limit_hit`（含 `until`）與「量表用完」，**不**把時間寫進窗口 `resets_at`；③ 同一張橫幅再掃到不算新證據（時間戳不前推）。
  **後到的結構化讀數不會清掉橫幅**（2026-09-13 晚改回）：codex 的 credits 用完時 5h／7d 這兩條**速率**視窗可以是滿的、app-server 也照實回報 0% 已用，
  唯一講出「現在收不下工作」的就是橫幅——那正是 `limit_hit` 這一格存在的理由。清掉它只有兩條路：`until` 到了，或下一回合真的跑完（`clear_limit_hit`，**只有 codex 走這條**：claude 的 Fable 用完換 opus 照樣能跑，成功回合不算解除）。
  所以 claude 這一側 `until` 是唯一的出口：橫幅指的那一桶還沒有讀數（daemon 剛重啟、statusLine 還沒進來）時，改用桶別的保底長度（session 5h、weekly／Fable 7d、認不出 5h）從撞上限的時刻起算，
  不再留下 `until=None`——那等於永遠不過期，交辦會卡在 `quota_blocked` 到有人重啟 daemon（review 2026-09-16）。
- controller 每 tick 掃：仍擋就順延；不擋了回 `queued` 立刻重送，用 `<client_request_id>#r<n>`（`lifecycle::prompt` 的冪等是同 crid 回同 turn，不換序號等於沒送）。
  「不擋了」要**兩個條件同時成立**：查不到未過期的 `limit_hit`，**而且** `resume_at` 已經到了。查不到讀數不等於額度回來了——`app.quotas` 只在記憶體（§12.4），
  daemon 一重啟就全空，只憑「沒有 limit_hit」重送會在開機瞬間把整批還在被擋的交辦倒出去（review 2026-09-16）。反過來，CLI 說還在擋但我們記的時間過了，是把 `resume_at` 往後挪。
- **開機回填**（`controller::backfill_quota_limits`）：控制器啟動時先用 parked 交辦的 `resume_at` 把該 host＋`quota_base` 的 `limit_hit` 補回記憶體（`quota::seed_limit_hit`，
  `source=parked-assignment`）。同一把 key 取**最晚**的 `resume_at`，已經過期的不寫；只寫 `limit_hit`，不碰任何量表或 `resets_at`。這樣重啟後 `dispatch` 也照樣看得到「這個帳號還在擋」。
- `assignment_quota_blocked` / `assignment_quota_resumed` 各推一則 inbox（`needs_review=false`），不開 incident。重送 6 次仍被擋 → `awaiting_review` + `turn_status=quota_exhausted`（通常是 credits 真的用完）。
  mission 交辦另有 `quota_policy` 與身份切換，見 §18.14。

### 18.9 總管健康與系統 incident

`manager_health`（AGM 能不能工作）與 `system_health`（系統有沒有壞）分開；頂層 `status` 取兩者較嚴重者。

incident 以資源為單位持久化（`supervisor_incidents`，`(kind, resource)` 在 `status='open'` 上唯一）：

| kind | 判斷 | 門檻（`[supervisor]`） |
| --- | --- | --- |
| `host_disconnected` | host 連不上 | `host_disconnected_secs`（120） |
| `bot_stopped` | `autostart=1` 的 bot 沒有 active run | `bot_stopped_secs`（300） |
| `assignment_stalled` | 未結案交辦 `updated_at` 沒動 | `assignment_stalled_secs`（7200） |
| `assignment_undelivered` | 仍 `queued`、從沒送出，用 `created_at` 算 | `assignment_stalled_secs`（7200） |
| `notify_exhausted` | 通知重送用盡 | `notify_max_attempts`（5） |

- 條件持續超過門檻才寫入（計時在記憶體，重啟重算——寧可晚開不重複開）；開啟與恢復各推一則 inbox，中間只更新 `occurrences`；恢復後再壞是新的一筆。
- `assignment_undelivered` 獨立一條：每次重試 `defer` 會推 `updated_at`，stalled 看不到它。被拒的真正理由（`bot has no active run`、`needs_login`）記在 `error`。
- 不算故障：使用者停掉的 bot、等使用者回答的 blocked、短暫排隊、AGM 自己的 idle/busy。量不到回 `unknown`，不併進 `healthy`。全部走 30 秒 cheap probe。

### 18.10 重建／重啟的核准與執行租約

「AGM 說可以」與「現在沒人在忙」都要落成紀錄，而且在執行期間持續成立：

1. **等安全窗口**：`GET /api/supervisor/maintenance/safety`，唯讀快照。
2. **取得排他窗口**：`POST /api/supervisor/leases/{resource}/acquire`，在同一個 supervisor lock 裡重驗核准與 idle，單一條件式 UPDATE 拿租約；搶同一窗口只有一個成功。

- 核准是紀錄：申請者、purpose、範圍、`target_commit`、有效期、決定者、理由。acquire 逐項核對（purpose 不符、過期、撤銷、commit 不同都拒）；release 時標 `consumed`——一次核准一個窗口。
- **申請可以帶穩定的 `request_id`**（AGM 裁示 2026-09-16）：2026-09-16 04:05 k8bw2f 對同一顆 `ca7b22d` 送了兩筆一模一樣的 restart 申請（它解析回應時取錯欄位，以為沒送成功），AGM 只能核一筆、駁一筆。
  現在同一個 supervisor 下同一個 `request_id` 再送回**原本那一筆**（`created:false`），不新增也不重推 inbox；**已經裁示的也回它本人**——重送的人要看到的是「這件事已經有裁示了」。
  同一個 id 換了 purpose／scope／commit 是兩件事共用一個 id：回 409 `approval_request_mismatch`，不回舊的也不覆寫。`expires_in_secs` 不參與比對。不帶就是舊行為。
  欄位是 additive 的 `client_request_id`（既有列留白，唯一鍵只約束有值的列），`approval list` 會帶出來對帳。
- 租約有只增不減的 `fence`；過期被接手後舊 fence 的 renew／release 一律失敗。到期自動釋放。acquire 與 renew 取「要求到期」與「核准到期」較早者，renew 重驗核准狀態。
- **憑證**（AGM 裁示 2026-09-16）：`acquire` 的回應多一個一次性的 `lease_token`（隨機 32 字元）。`renew`／`release` 必須帶對的 token，否則 **403**（`lease_token_required`／`lease_token_mismatch`），租約一個字都不動並留一行 warn。
  token **只在 acquire 的回應裡出現一次**：`lease status`、`GET /api/supervisor`、WS 事件與 `Lease::to_json()` 都不含它。owner 與 fence 是公開欄位，光憑它們等於誰都能把別人正在換 binary 的窗口收掉——那一刻正好最不能被打斷。
  例外只有兩條，都留稽核：(a) daemon 啟動時釋放上一輪殘留的 `restart` 租約（現行行為不變）；(b) **AGM 角色**（patrol／responder，`X-AM-Bot-Id`＋該 bot 自己的 hook token 驗出來的）的強制釋放 `--force`：`reason` 必填，寫進 `supervisor_notes` 的 `lease_force_release`（含判定到的角色），log warn，且不比對 owner／fence——持有者可能已經不在了。
  **不是 AGM 角色帶 `force` → 403 `lease_force_forbidden`**，租約一個字都不動：force 是授權問題，不只是稽核問題；只留紀錄而沒有界線，等於誰都能接管別人正在換 binary 的窗口。
  升級過渡：欄位是 additive，升級前建立的租約 `lease_token IS NULL`，這種**不帶 token 也能釋放**（否則升級當下握著的窗口永遠沒人還得了），釋放時留一行 warn。過期自動失效與 fence 只增不減的語意都沒變。
  ops 腳本把 token 寫進派工正文（執行者要用它交還窗口）：那是一個有效期最多一小時的窗口憑證，不是帳號憑證。
- 持有 `restart` 租約期間 **supervisor 的 assignment 派送 hold**（留 `queued`，不算重試）。**只管這一條通道**：`POST /api/bots/{id}/prompt` 沒被 gate。
- **重啟後無等待期**：窗口在租約 release（API）或 daemon 啟動完成（開始 listen 後自動 release 仍未釋放的 `restart` 租約、consume 核准並記 info log）時就結束，被它 hold 的交辦立刻解除、controller 下一輪（≤10 秒）直接派送，不等 hold 寫的到期時間；controller 每輪派送前發現已沒有 held 的 `restart` 租約（含到期）也會先解除殘留 hold。
- 安全窗口 fail closed：讀不到某顆 bot 狀態回 `safe:false` 並列在 `unreadable`。`restart` 不接受 `require_idle=false`。
- **等太久就縮小封鎖面**（AGM 裁示 2026-09-16）：在這台機器的負載下「任何 bot 在回合中就不換」等同永遠不安全——2026-09-15 那筆核准卡了 11 小時，每 5 分鐘那一輪都撞到有人在講話。
  所以同一筆**已核准、未消耗**的 `rebuild`／`restart` 申請，從**核准時間**（`decided_at`）起連續等超過門檻（常數 30 分鐘，`AM_MAINTENANCE_ESCALATE_MINS` 可調；0、負數或看不懂的值當沒設）之後，安全窗口改判「縮小封鎖面」：
  - **誰等太久就放寬誰**（AGM 裁示 2026-09-16）：`acquire` 只看**當下這筆核准自己**等了多久，別人放著沒用掉的核准不算數——否則一張被遺忘的核准等於把所有人的窗口都打開。
    唯讀的 `safety` 帶 `?approval=<id>`（CLI `agm lease safety --approval <id>`）時同樣只看那一筆；不帶（純查詢，還不知道會用哪一筆）才退回看最早那筆還活著的核准。回傳的 `escalation_approval_id` 就是這次計時用的那一筆。
    認不得、已消耗、被撤、過期或還沒決定的核准一律不計時（＝不放寬）。
  - **仍然擋**：送達臨界區（`turns.status='queued'`，或 `status='in_flight'` 且 `delivery='pending'`——daemon 正在往 pane 打字／送出）、任何**還握著**的租約、讀不到狀態的 bot（`unreadable`）。
  - **不再擋**：bot 只是在 `working`／思考（已送達的 `in_flight`）。`blocked` 照舊只回報不擋。`delivery='unknown'` 是停在那裡等人處理的狀態，不算臨界區。
  - AGM 三顆（巡檢、協調者、建置 child）照舊由呼叫端排除，門檻高低都一樣。
  - **留痕**：safety 多回 `escalated`、`waited_secs`、`escalation_approval_id`，以及 `delivering`／`held_leases` 兩份清單；acquire 把整份 safety 寫進租約 meta 並在 log 明寫「升級後才拿到窗口」；`daemon-update-kick.sh` 的 log 與派工正文也寫明這次是升級後才換的。
  沒等超過門檻時**完全不變**：全靜止才 `safe`。
- assignment 可帶 `ownership`（檔案／模組），重疊時 `POST /assignments` 回 `ownership_conflicts`，**只回報不阻擋**。
  「未結案」只有一份定義（`store::OPEN_STATES`，六個狀態），ownership 衝突與未結案計數都從它產生——以前四個查詢各自硬寫清單、三種答案，
  `quota_blocked` 因此從衝突檢查裡消失：AGM 查過衝突、回報「沒有人握著這塊」，然後把同一個模組派給第二顆 bot（review 2026-09-16）。
  「卡住沒人管」是另一張具名的表（`STALLED_STATES`），刻意不含 `quota_blocked`（在等一個已知時間點）與 `blocked`（在等人回答）。
- 前一個持有者**過期**而不是 release 時，接手的那次會把舊租約的核准標成 `consumed`（理由 `lease expired`，寫進 `supervisor_notes`）：
  否則同一張「可以」能在有效期內開好幾個窗口，而決定歷程上一筆紀錄都沒有。consume 一律走 `decide_approval_from`（有稽核、不覆寫 `decided_at`，升級判定的計時看的就是那一欄）。
- 例行更新腳本的順序是**先取回／申請自己的核准，再問 `lease safety --approval <id>`**：升級是綁在那筆核准等了多久，
  不帶就是用「最早那筆還活著的核准」判斷自己要不要繼續，升級在這條路上等於死碼（review 2026-09-16）。
- 運維腳本在 `scripts/ops/`，附隔離測試（`scripts/ops/daemon-update-kick_test.sh`，假 CLI + 暫存 repo）。
- 邊界：租約只約束走 API 與這些腳本的路徑，shell 仍可直接 kill daemon 或 `cargo build --release`。租約讓「問過 AGM」在執行期間持續成立，不取代它。

### 18.11 人設的權威、版本與建置依賴

人設的副本：repo 內嵌版、`supervisors.persona_text`、`bots.persona`、`config.toml`、總管 cwd 的 `persona.md`、session 已載入版。

- **持久版（`supervisors.persona_text`）是權威。** `setup` 首次遷移保留既有 `bots.persona`；完全沒有人設才以內嵌版 seed 一次，之後 seed 一律被拒（否則舊 binary 跑一次 setup 就把人設降回它編進去的版本）。
  `persona_seed_hash` 記住 seed 自哪個內嵌版。
- 內嵌版取代持久版只有 `POST /api/supervisor/persona/adopt-embedded`（有 actor 與理由）。
- `config.toml` 與 `persona.md` 是從持久版產生的副本；改人設走 `PUT /api/supervisor/persona`（可帶 `expected_version` 樂觀鎖）。
- **已載入版不可觀測**：daemon 只在啟動 CLI 時傳 persona，看不到 session 現在握著什麼。`loaded.status` 只有 `unknown`／`stale`／`unverified`，沒有 `verified`；
  `needs_restart=false` 只代表不是舊 session。
- **建置依賴**：`GET /api/supervisor/build-inputs` 列出會進 binary 的路徑（含 `include_str!` 的 `docs/goals/agm-supervisor-persona.md` 與 `scripts/agm.py`）；這些改了 binary 就落後，
  但何時重建、何時重啟由 AGM 分開判斷。清單與實際 `include_str!` 由測試綁住（`every_embedded_file_is_declared_as_a_build_input`）。

### 18.12 遠端（手機）入口的可觀測性

AGM 是使用者唯一的手機入口，但 `--remote-control AGM` 只是 argv 上的**要求**；herdr pane 狀態、hook payload、session 資料都不帶 Remote Control 資訊，
**目前沒有可靠的觀測來源**，所以 `capability.status = unsupported`，daemon 不宣稱手機已連上。

- 狀態只有 `requested`／`verified`（有可驗證來源）／`unavailable`（有證據說不通）／`unknown`，沒有 `active`。argv、bot 自己的文字、URL 都不能推出 `verified`。
- 觀測綁 session（run id）且 900 秒過期；AGM 重啟或換模型後舊確認失效（`revoked: session_changed`、`observation_expired`），撤銷時不再回 `url`。
- 人工確認走 `POST /api/supervisor/remote`，`source=manual` 且**必須帶 actor**（記成「某人宣稱過」，不是量測）。
- incident 只在 `unavailable` 時開。恢復 remote session 由 AGM 在沒有回合衝突的窗口安排，daemon 不自己多開 session。

### 18.13 回滾限制
supervisor 相關資料表與欄位都是 additive，`db::migrate` 重跑冪等。往回滾到較舊的 binary 時：
- 舊 binary 不認得 `awaiting_review`／`blocked`／`superseded`／`quota_blocked`，這些交辦會從它的未結案清單消失（資料不刪）；回滾前先逐筆決定掉。
- 舊的 `on_turn_done` 會把跑完的交辦直接寫成 `completed`，且不會標 `legacy_closed`。
- 舊 binary 不看租約（restart 窗口不 hold 派工）；舊的 `ensure_env` 會用內嵌版覆寫人設——回滾前先確認內嵌版就是要的那份。

### 18.14 群組任務（mission）的 AGM runbook（2026-09-13）

使用者決策見本節末的 D1–D8，API 契約 `docs/API.md`「群組任務」。daemon 只做確定性的部分（任務／事件持久化、
身分挑選、輪數上限、fast-forward 交付）；下面是 AGM 這一側的步驟，每一步都用 `bin/agm mission …`／`bin/agm assign --mission`，
不拼 curl。一個任務同時只有一件開著的交辦；`phase` 由交辦推導，AGM 不另存狀態。

1. **收到 `mission_created`**（inbox）：讀 `mission get`。指示不清或範圍太大 → 在群組問使用者（`mission event --kind note`
   ＋ `mission pause --reason clarify`），不猜。與其他未結案交辦的 ownership 重疊 → 先排隊，在任務記 `note`。
2. **執行者**：`mission pick --role executor` → `use` 就用該身分（`model` 有值要換模型）開臨時 bot（乾淨 worktree，命名
   `agm-mission-<id 尾 6 碼>-exec`），`assign --mission <id> --role executor`，文字含：指示原文、cwd、ownership、完成條件、
   「推 origin/main 由 AGM 交付，執行者只推 task branch」。`wait` → 什麼都不做，controller 會依 `quota_blocked` 續派；
   `ask_user` 只會出現在驗證者。
3. **reviewer**：執行者回合結束並 `review accept` 後，`mission pick --role reviewer --exclude <執行者身分>`；
   `no_independent_reviewer` → 跳過 reviewer、記 `note`「執行者自審＋驗證者把關」。reviewer 只讀 diff，回 `am-review`
   （`approve|changes`＋findings）。`changes` → `mission round`（409 `max_rounds` 就停，任務已 `paused`，在群組問人）
   → 對執行者那件 `review followup`，文字帶 findings。
4. **驗證者**：`mission pick --role verifier`；`ask_user` 時任務已停在 `no_fable_for_verifier`，在群組問使用者要等哪個身分
   或改用非 Fable，**不自行降級**。`use` → 臨時 bot 在乾淨 worktree 跑 repo 規定的驗證（本 repo：`cargo test`、
   `tsc -p tsconfig.app.json`、oxlint、build、UI 截圖），回 `am-verify`；通過 → `mission event --kind verified`（帶數字與截圖路徑）；
   失敗 → `mission round` → followup 退回執行者。
5. **交付**：`mission deliver --worktree <執行者 worktree>`。409 `not_verified` 代表流程漏了第 4 步；其餘 409 任務已
   `paused`（`push_main_failed`／`pr_failed`，`reason` 是機器碼），在群組貼原因問人，**不 force、不自己 rebase 後硬推**。
   交付含 daemon／agm.py／persona 改動時，正式 daemon 照 §18.2 例行更新，不另開重啟。
6. **回報與收尾**：`mission complete <id> --text …`（或 `--text-file`；內容是結果摘要：commit／PR、驗證證據、輪數），
   群組時間軸由 daemon 記 `completed`。daemon 在 complete／cancel 時會**自動軟刪**這個任務的臨時 bot——條件是它是任務某件交辦的
   目標、名字以 `agm-mission-<id 尾 6 碼>-` 開頭、而且沒有進行中的 run；回應的 `temp_bots.skipped` 列出沒刪的與原因。
   `still_running` 的那幾顆先 `bot stop <id>` 再 `bot delete <id>`；刪除保留對話紀錄與 `mission_events` 作證據。
   所以第 2、4 步開臨時 bot 時**一定照這個命名**，否則收尾時不會被認出來。
7. **撞額度換手（`mission_identity_switch`）**：那件交辦停在 `awaiting_review`／`identity_switch`。用 `to_identity`（與
   `model`）開新臨時 bot，對原交辦 `review followup`，文字帶進度摘要（已做／未做／未提交檔案、worktree 路徑）；
   followup 會沿用 `mission_id`／`role`。同身分同模型的 `wait` 由 daemon 自己重送，AGM 不介入。
8. **停下問人的統一原則**：`paused_reason ∈ max_rounds | no_fable_for_verifier | push_main_failed | pr_failed | clarify`
   都是問使用者一個具體問題，得到答案後 `mission resume` 再從對應步驟接續；使用者取消 → `mission cancel`。
9. **不做的事**：不代使用者回答問卷；不在一個任務裡同時開兩件交辦；不用 `/loop` 輪詢任務（`mission_updated` 與 inbox
   事件會來）；臨時 bot 不開 remote。
10. **實跑教訓（2026-09-13 兩個任務）**：
   - 驗證者的截圖**不要放在執行者的 worktree**（deliver 會 409 `dirty_worktree`）；放 AGM 的 scratchpad 或另一個目錄，路徑寫進 `verified` 事件。
   - `not_fast_forward` 不算「停下問人」：對執行者**新開**一件 `assign --mission --role executor`（已結案的交辦不能 `followup`）
     要它 `git rebase origin/main` 並重跑 tsc／build／test；rebase 乾淨就直接再 `deliver`，**有衝突才**停下問人。
   - `mission complete` 只會自動刪「沒有 run」的臨時 bot；idle 但 run 還活著的會被 `still_running` 跳過，收尾照第 6 步先 `bot stop` 再 `bot delete`。
11. **完成之後的追問與追加修改（2026-09-13，AGM 裁示 01M2D18PQZSJ4Z5BJC21TF9Q77）**：交付完不是句點，使用者還會
    問問題、還會想再改一點。兩條路刻意分開，因為後果不同：
   - **追問**（`mission question`，inbox `mission_question`）：只是問一句話。你回 `mission answer <id> --reply-to <question 事件 id>`
     （CLI 會自動帶你自己的 `relay_from`）。**不要**因為一句追問就去改碼、開交辦或動交付——要改東西是下一條。
     已完成的任務也能問；它不會把任務弄回進行中，`completed_at` 與成果摘要都不會變。
   - **追加修改**（`mission revise`，inbox 是一則帶 `parent_mission_id` 的 `mission_created`）：那是**新的一筆任務**，
     舊那筆原封不動。payload 帶 `runbook_start_step: 2`——脈絡（原指示、結果摘要、commit／PR、`verified` 摘要、新要求）
     已經在快照裡，你從第 2 步（挑執行者）接手，不用重新規劃。臨時 bot 用**新任務**的 id 尾 6 碼命名。
   - 快照是**參考，不是證據**：新任務要自己跑第 4 步拿到自己的 `verified` 才能交付，舊的驗證對它無效
     （交付關卡看的是這筆任務自己的事件）。快照會帶原成果的 `verified` 事件**連同它的 payload**（截圖路徑、
     測試數字都在裡面）。
   - 原成果現在在不在基底裡，daemon **不知道**：`parent_delivery_in_main` 一律是 `"unknown"`，只有
     `parent_delivery_mode` 說得出當初用哪種方式交付。push 過的可能被 revert，PR 也可能早就合併了——
     動手前自己查目前基底，兩個方向都不要假設。
   - 一個成果同時只能有一輪未結案的續作（第二筆會被 409 `revision_in_progress` 擋下並指向既存那筆）。
   - 使用者回答暫停的任務（web 直接回答；CLI 依使用者明確指示代送時用 `mission answer --as-user`，不帶 `--reply-to`）或按「不回答直接繼續」（`mission resume`），
     daemon 都會推 inbox 叫醒你；你自己呼叫 resume 不會產生通知。


**使用者決策（群組任務的需求本身，不是實作細節）**

- D1 「已完成任務」＝專案底下一份可回顧的完成清單（原始指示、結果摘要、commit、驗證證據）。
- D2 交付方式由使用者每個任務選：直接推 main，或開 PR。
- D3 驗證者必須用 Fable 模型，且該身分的 Fable 週桶還有有效額度。
- D4 帳號用盡才換下一個（cc2 → cc1 → cc0），必須能偵測撞限並切換。
- D5 5h 撞限時等重置還是直接換身分，開任務時選（`on_5h_limit = wait|switch`）。
- D6 驗證者找不到 Fable 有效額度 → 停下來問使用者，不自動降級、不乾等。
- D7 執行者 kind 開任務時選（claude／codex／grok）；帳號輪換只對 claude 有效。
- D8 推 main 失敗（非 fast-forward、rebase 衝突、驗證沒過）→ 停下來問使用者，不自動改開 PR。


### 18.15 AGM 雙角色：巡檢與協調（2026-09-13）

使用者 2026-09-13：主動找問題用 fable-low（不足 opus-low），回應 bots 用 opus-high。在那之前每個 bot 申請、交辦回報、核准請求、健康事件都打進同一顆 AGM，
而它跑在 fable 上——bot 的例行申請與十分鐘一次的 `health_changed` 是 fable 用量最大的來源。

| | 巡檢（patrol） | 協調（responder） |
| --- | --- | --- |
| 是誰 | 原本那顆 AGM：`supervisors.bot_id`、`GET /api/supervisor` 的頂層欄位 | 第二顆 bot `AGM-responder`：`supervisor_roles` 的 `responder` 列 |
| 模型 | cc0/fable/low → cc0/opus/low（§18.5 的控制器） | cc0/opus/high，固定，沒有自動切換（`responder setup` 可指定） |
| 入口 | 使用者 web／手機 Remote Control（**唯一**的 remote） | 沒有 remote；只有 daemon 的通知 |
| 目錄 | `supervisor/AGM` | `supervisor/AGM-responder`（記在 `bots.cwd`；claude session 以 cwd 為鍵，共用會互相接到對方的 session 與 `persona.md`） |
| 專案 | 巡檢的專案 | **同一個**（見下）；側欄上兩個角色在同一塊 |
| 收什麼 | `health_changed`、`incident_*`、`watchdog_gave_up`、`bot_restart_failed`、`supervisor_restart_retry`、`responder_watchdog_gave_up`、`review_role=patrol` 的交辦回報、不認得的種類 | `bot_request`、`approval_requested`、`mission_*`、其餘交辦回報 |
| 喚醒節流 | `notify_interval_secs`（600） | 短窗批次 `responder_batch_secs`（15）：最舊的待辦等滿、且距上次喚醒也滿才叫 |

**協調者的健康算進頂層 `status`**（review 2026-09-16）：它是 bot 申請、核准請求與所有 `mission_*` 的唯一收件人，
以前 `status` 只取巡檢與系統兩半的較差者，協調者 `waiting_quota` 或倒掉時使用者入口仍顯示 `healthy`、沒有任何人被叫醒，
申請可以躺好幾天。`responder_health` 那一格照舊分開列。`responder_bot_missing` 的 event_key 也加了小時格，
不再是「一輩子只提醒一次」（`push_inbox` 是 `INSERT OR IGNORE`）。

**一顆總管、一個專案**（使用者 2026-09-16）：協調者最早自成一個專案，因為一個專案只有一個 path；但側欄上「AGM」與「AGM-responder」分成兩塊看起來像兩顆總管。
現在協調者的 bot 掛在巡檢的專案底下，工作目錄改由 `bots.cwd` 表達（`lifecycle::bot_cwd` 先看它，再退回專案的 path）——目錄仍然分開，只是不再自成一個專案。
`responder::merge_into_manager_project` 在 daemon 啟動與 `responder setup` 時各跑一次，把舊安裝搬過去：同一顆 bot（id 不變，對話歷史不斷）、
空掉的舊專案只在**路徑對得上協調者目錄**時才拿掉，可重入；巡檢還沒設定專案時什麼都不動。

**路由由 daemon 決定**（`supervisor/roles.rs::route`，純函式），只看事件種類、payload 明寫的欄位與交辦的 `review_role`，不問模型、不比對名字；
不先叫醒巡檢再請它轉交。每筆 inbox 事件記 `role`、`wake`、`claimed_by`、`acked_by`、`merged_into`。

- **只記錄、不叫醒**（`wake=0`）：`assignment_noticed`、`quota_blocked`／`quota_resumed`、`incident_resolved`、`manager_health.status=healthy` 的 `health_changed`、
  bot 對通知型交辦（`--notice`）的回覆、角色之間在同一次喚醒回合裡的回信。它們跟下一次有事的喚醒一起送，自己不開回合。
- **巡檢送前合併**：還沒送出的 `health_changed` 只留最新一筆；同一個 incident 在送出前就開了又恢復，兩筆一起結案（`acked_by=daemon`）。
- **bot 找 AGM**：`POST /api/bots/{巡檢或協調者}/prompt` 帶 `relay_from=<bot>`、或 pane 裡 `herdr agent prompt <AGM>`（shim 先打 `/relay/announce`），
  協調者建立後都**不開回合**：寫成 `bot_request`（202，`routed`），shim 看到 `routed` 就不打進 pane。
  去重鍵：有 `client_request_id` 用它，沒有就用寄件者＋內容指紋＋十分鐘一格。指紋 = 收件角色＋目標＋正文（逐字，不做空白正規化，縮排差一格就是不同內容）＋附件；
  同一個 id 換了內容不是重播，回 409 `request_mismatch` 且什麼都不寫——否則第二次申請會被讀成「送到了」而靜靜消失。
  一般 bot → 協調者；巡檢 ↔ 協調者互相交接給對方。不攔：使用者（沒有 `relay_from`）、`relay_from=daemon`、目標不是角色 bot、協調者未建立。
- **角色之間的交接**：`assign` 的目標是另一個角色 bot 時，**不是交辦**——不建交辦列、不開回合，而是走同一條佇列
  （回 `{kind:"handover", routed, queued, duplicate, wake, inbox_event_id}`），批次、節流與「回覆不再叫醒對方」只有一份規則。
  對自己的角色下交辦仍是 400。排隊中的交辦若目標在期間變成角色 bot，`dispatch` 停手並記 `dispatch_failed`，不直接打進對方 pane。
- **升級時舊事件歸誰**：第一次加 `claimed_by` 欄時，與回填**同一個 transaction**（回填失敗連欄位一起回滾，重啟會再做一次）。
  只有送達痕跡（`notify_turn_id`、`delivered_at`、`state='delivered'`）的舊事件記給巡檢；`notify_attempts>0` 不算——
  `defer_notify` 在完全送不出去時也會加一。先前版本曾把「只有嘗試次數」的 pending 誤記給巡檢；migrate 每次都會把
  「還在 pending、`claimed_by='patrol'`、沒有任何送達痕跡、沒人 ack」的列放回路由表（`mark_delivered` 寫 claim 時一定
  同時寫送達痕跡，所以這種列只可能出自那次回填），真的被角色收走或結案的工作一律不碰。
- **一件事只有一個角色**：擁有者的定義是 `COALESCE(claimed_by, role)`——送出去之後看實際收的人，還沒送就看路由表。
  兩個角色的待送查詢、`ack` 的守衛與 UI 過濾都用這一條，所以雙角色剛啟用時、先前由巡檢收走（`claimed_by='patrol'`）
  而被 recover 放回 pending 的協調事件仍歸巡檢，不會被協調者撈去送、卻又寫不進 delivered（每個 tick 重送一次）。
  送出時以 `claimed_by` 條件更新；`ack` 的守衛跟寫入在同一句 SQL（先讀後寫之間 claim 會變），帶角色 bot token 時只能結自己收的（另一個角色的回 409 `claimed_by_other_role`），UI／使用者照舊全能結。
  核准決定改為條件寫入（`WHERE status=<讀到的狀態>`），兩個角色同時決定只有一個成功（409 `decided_concurrently`）；交辦驗收本來就是條件寫入。
- **角色身分**：只認 `X-AM-Bot-Id` + 該 bot 的 hook token（`X-AM-Bot-Token`）。`bin/agm` 在自己的 pane 裡（`AM_BOT_ID` 等於 runtime 的 `self_bot_id`）才帶；
  驗證過的決定記成 `AGM:patrol`／`AGM:responder`，body 自稱的 `actor` 不算。`relay_from` 的 bot 申請沒帶 token 仍收，但標 `sender_verified=false`。
- **協調者故障不倒回巡檢**：分流只看它**建立過**沒有（`supervisor_roles.responder` 的 `bot_id`），不看它現在活不活著。沒額度（CLI 撞限，或共享 5h／7d critical）→ `status=waiting_quota`、`notify_next_at`＝重置時間與上限取早者，事件留 `pending`、不計重試次數；
  停著 → 看門狗（同 §18.9 的 30/60/120/300 秒、5 次）；放棄 → 推 `responder_watchdog_gave_up` 給巡檢。送不出去是有界退避（15 秒倍增到 `responder_max_backoff_secs`），**沒有次數上限**，
  也不開 `notify_exhausted`。巡檢自己的事件照舊有 `notify_max_attempts`。
  登記的 bot 被刪掉 → `status=missing`（`configured:true`、`bot_present:false`），推一次 `responder_bot_missing` 給巡檢，事件照樣留在協調者的佇列等它被建回來。
- **舊部署**（協調者未建立）：協調的事件由巡檢照 600 秒節流收，行為與之前相同；建立之後才分流。已送給巡檢的舊事件仍歸巡檢。
- **上次成功上線**：`GET /api/supervisor` 的 `last_deploy{sha,at}`——sha 由 `daemon/build.rs` 在建置時編進 binary，
  `at` 是這個 process 起來的時間。origin/main 動了不等於上線了，這一格說的是「現在跑的是哪一版」。
- **交辦的來源綁呼叫者**：`POST /supervisor/assignments` 記的「使用者原話」只認**呼叫的那個角色自己**的回合
  （`X-AM-Bot-Id`+token 驗過）。兩個角色同時在回合中時，照固定順序先撿巡檢的回合，會把協調者派的工記成
  「使用者對巡檢說的另一句話」，授權與稽核從此對不上人。明講 `source_turn_id` 也只能指自己的回合；
  認不出呼叫者（UI／腳本）就不猜，記 `assignment_text_fallback`。
- **核准只有第一個裁示算數**：`approve`／`deny` 只從 `pending` 條件寫入（不看先前讀到的值，`op_lock` 不是
  唯一防線）；已決定的回 409 `already_decided`，並列出 `allowed_from`。翻案要明講 `revoke`（從 `approved`
  或 `pending`），決定歷程 append-only 記在 `supervisor_notes`（`kind=approval_decision`），
  `GET /supervisor/approvals` 每筆附 `decisions`。撤銷之後不能就地再核准，要開新的一筆申請。
- **協調者的額度只看自己的帳號，而且分三態**：key 走 `quota_base_for_host`（`cc0` 這種 env 空的身分讀裸 `claude`，
  有自己 env 的 cc1／cc2 只讀自己那把），主機要確定——查不到專案或主機欄讀不出來就當不知道，不退回本機借數字。
  狀態是 Unknown／Available／Blocked：`Available` 要 5 小時與 7 天兩格都有讀數、都沒見底、也沒撞限；空的或不完整的讀數
  是 Unknown。已知的 `waiting_quota` 只有兩種證據能解除：可信的 Available 讀數，或協調者在**開始等待之後**答完了一個
  沒留 `turn_error` 的回合（`supervisor_roles.waiting_since`）。prompt 送達（`ok`／`unknown`）**不算**——那只代表字進了
  pane 或佇列，CLI 可能下一刻才報撞限；等待期間送出後只把下一次重試推到有界間隔之後。額度狀態每個 tick 重算，
  撞限期間沒有新讀數不改 `notify_next_at`，也不重寫 DB、不推事件。
- **Remote Control 明講在每顆 bot 的設定檔**：`claude-settings.json` 一律寫 `remoteControlAtStartup`，值就是
  「這顆 bot 的 argv 有沒有 `--remote-control`」。使用者帳號的全域 `settings.json` 開了它的話，原本**每一顆**
  bot 起來都會多開一個手機入口——協調者的 rc off 不能只靠 `args=[]`。
- **協調者的 model／effort 會變成 argv**：`responder setup` 驗形狀（模型 `[a-z0-9][a-z0-9._-]{0,39}`、effort 走
  `config::normalize_effort`），擋掉旗標、空白與超長字串；不釘死型號，CLI 換代不必改。`GET /supervisor/responder`
  的 `model`／`effort` 是設定值，`runtime{model,effort,started_at}` 才是它現在實際跑的。
- **交辦的驗收角色**：`POST /api/supervisor/assignments` 的 `review_role`；省略 = 呼叫的角色（token）自己，UI／腳本呼叫 = 協調者。巡檢的例行維運（daemon-update、browser-gc、健康追查）寫 `patrol`。
  followup 沿用父交辦的 `review_role`。協調者存在且驗收角色是它時，派工訊息的 `relay_from` 標協調者，bot 回話才找對人。
- **計數**：每個角色 `wakes`、`events_delivered`、`duplicates`（擋下的重複申請）、`merged`、`last_wake_at`、`last_wake_reason`（這一批的事件種類）。
  「fable 不會因為 bot 申請被叫醒」看巡檢的 `wakes` 與 `last_wake_reason` 裡沒有 `bot_request`。
- **健康**：`GET /api/supervisor/health` 另有 `responder_health{status,responder_status,inbox_open,retry_at}`，不併進頂層 `status`（協調者等額度不等於使用者入口不能用）。
- **限制**：bot 繞過 shim 直接用真的 herdr 打進巡檢 pane、或 daemon 不在時 shim 退回直送，daemon 看到的是外部回合（當成使用者），會吃巡檢一回合。
  協調者在自己的專案，web 的「剛跑完」晶片列目前只排除 `GET /api/supervisor` 的 `project_id`（巡檢專案），協調者會出現在那一列。
- **部署**（合入 main 後由 AGM 安排，不在程式裡自動做）：`agm responder setup` → 同步兩份 persona（§18.11，協調者走 `PUT /api/supervisor/responder/persona`）→
  `agm responder start`。回滾到舊 binary：新欄位是 additive，舊 binary 忽略 `role`／`wake`，所有事件回到巡檢收；先停協調者。

## 附錄 A：herdr socket（0.8.2 / protocol 20）

- 每個請求一行 `{"id":"<string>","method","params"}`，`id` **必須是字串**；回應 `{"id","result":{"type":…}}` 或 `{"id","error":{"code","message"}}`
  （`agent_not_found`、`workspace_not_found`、`pane_not_found`、`agent_not_ready`、`agent_blocked`、`invalid_request`）。
- 一條連線一個請求，回應後伺服器關閉；`events.subscribe` 例外，先回 `subscription_started` 再持續推事件行。
- `pane.agent_status_changed` 訂閱必須帶 pane_id；一次 subscribe 可帶多個 filter。全域 `pane_updated` 不穩定反映 agent 狀態，不可當狀態來源。
- `agent.start` 非同步（`launch_pending:true`），需 `agent.wait {until:[idle,done,blocked]}`。`agent.read` 的 `source`：`visible | recent | recent_unwrapped | detection`。
- `session.snapshot` 回 `{version, protocol, workspaces, tabs, panes（含 agent、agent_status）}`；agent name 另以 `agent.list` 取。
- named session：`herdr --session <name> server`，socket `~/.config/herdr/sessions/<name>/herdr.sock`。
- pane env 由 herdr 注入 `HERDR_ENV=1`、`HERDR_PANE_ID`、`HERDR_SESSION`、`HERDR_SOCKET_PATH`、`HERDR_TAB_ID`、`HERDR_WORKSPACE_ID`。
- 從 Claude Code 內啟動的 daemon，pane 會繼承 `CLAUDE_CODE_CHILD_SESSION` 使 claude 不存 transcript → env 覆寫為空字串。
- 契約參考 `docs/herdr-schema.json`。

## 附錄 B：agent hook 事實

- Claude Code：`--settings <abs>` 注入 hooks；Stop stdin 見 §4.1。**Stop hook 的 stdout 若是 JSON 會被當決策**，子命令必須空 stdout。
- codex：`-c 'notify=[…]'` 覆寫 notify，argv 最後一項是 JSON（§4.1）。
- 資料表 schema 以 `daemon/src/db.rs` 的 migration 為準。

## 附錄 E：遠端環境事實

- 非互動 ssh shell 的 PATH 只有 `/usr/bin:/bin:/usr/sbin:/sbin`（herdr 在 `/opt/homebrew/bin`，claude/codex 在 `~/.local/bin`），所以要 `remote_path`；pane 內是互動 shell，PATH 正常。
- `ssh -N -M -S <ctl> -L <local.sock>:<remote herdr.sock>` 轉發後本機 `HerdrClient` 全部 RPC 與事件訂閱可用；AF_UNIX 路徑過長會 `path too long`。
- **Keychain**：ssh 下 `security find-generic-password -s "Claude Code-credentials"` 失敗（36），同一指令在 `launchctl bootstrap gui/<uid>` 的 LaunchAgent 內成功。
  只有 Keychain 憑證（沒有 `.credentials.json`）的身份在 ssh 起的 herdr 底下會「Not logged in」。
- 遠端 claude 首次在某目錄啟動會出 trust 提示（`blocked`），游標在「No, exit」，要 `down` + `enter`。
- 使用者權限的測試 sshd：`/usr/sbin/sshd -f <cfg>`（`Port 2222`、`ListenAddress 127.0.0.1`、自產 HostKey、`AuthorizedKeysFile`、`StrictModes no`、
  `AllowStreamLocalForwarding yes`、`StreamLocalBindUnlink yes`、`UsePAM no`），不需 sudo。
- herdr 官方 integration（`herdr integration install claude|grok`）只在 SessionStart 呼叫 `pane.report_agent_session`，狀態交給終端偵測，`seq` 用 `time.time_ns()`。

## 附錄 F：grok CLI 事實

### F.1 CLI
- `--always-approve`（= `--permission-mode bypassPermissions`）、`-m <model>`、`--reasoning-effort`（alias `--effort`）、`-p` 單回合、`-r/--resume`、`--rules`、`--cwd`…
- **不存在**：`--settings`、`--hooks`、TUI 的 `--plugin-dir`（只在 `grok agent …` 子命令）。

### F.2 hook 機制
- 來源全部合併：`~/.grok/hooks/*.json`（全域、永遠信任）、`~/.claude/settings.json` 相容掃描、`<project>/.grok/hooks/*.json`（需 `/hooks-trust`）、`config.toml` `[[hooks.<Event>]]`、plugin。
- 事件：`SessionStart`、`SessionEnd`、`UserPromptSubmit`、`Stop`（可 block）、`StopFailure`、`StopCancelled`、`PreToolUse`、`PostToolUse`、`Notification`、`SubagentStart/Stop`、`PreCompact/PostCompact`。
- command 經 shell 執行，事件 JSON 從 **stdin**（argv 空）；注入 `GROK_HOOK_EVENT`、`GROK_SESSION_ID` 等，**父程序 env 會繼承**（`AM_BOT_ID` 從 pane env 傳到 hook）。
  Stop 預設 timeout 600 秒、其餘 5 秒；fail-open；Stop 的 stdout JSON 當 decision，exit 2 會 block。
- payload 鍵為 camelCase（另附 snake_case 副本）：

```json
// session_start（TUI 下延遲到第一次 prompt 才觸發）
{"hookEventName":"session_start","sessionId":"…","cwd":"…","workspaceRoot":"…","permissionMode":"bypassPermissions","source":"new"}
// stop（回合結束）
{"hookEventName":"stop","sessionId":"…","transcriptPath":"…/updates.jsonl","promptId":"…","reason":"end_turn","stopHookActive":false,"lastAssistantMessage":"GROK-OK"}
// stop（session 結束時再來一次）/ session_end
{"hookEventName":"stop", …, "reason":"shutdown","stopHookActive":false}
```

### F.3 終端與偵測
- 回覆在終端無標記，右側帶時戳與捲軸：

```
     ❯ Reply with exactly GROK-OK                                    2:09 AM
     ◆ user_prompt_submit  [hooks: 1]                                        █
     GROK-OK                                                         2:09 AM   █
     Worked for 3.6s                                        stop  [hooks: 2]   █
  Help improve Grok                                       [Opt out] [Opt in]
  ╭──…──╮ / │ ❯ … │ / ╰── Grok 4.6 (low) · always-approve ─╯
  Shift+Tab:mode  │  Ctrl+.:shortcuts
```

- herdr 的 grok manifest：blocked 靠 OSC title 含 `Action Required`、`(○) Yes, proceed` 選單、頁尾 `:select │ ctrl+o:yolo │ ctrl+c:cancel`；
  working 靠 OSC 9;4 `4;1;-1`、braille spinner `[stop]`、頁尾 `esc:cancel`；idle 靠 OSC title `grok` / `<session> - grok`、頁尾 `ctrl+.:shortcuts`。

### F.4 身份隔離
`GROK_HOME` 覆寫設定目錄（`config.toml`、`auth.json`、`sessions/`、`hooks/`、`plugins/`、`memory/`），沒有 `GROK_CONFIG_DIR`。其他 env：`XAI_API_KEY`、`GROK_SANDBOX`、`GROK_FOLDER_TRUST=0`。
