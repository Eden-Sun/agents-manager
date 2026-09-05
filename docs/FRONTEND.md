# 前端（`web/`）說明

Vite + React 19 + TypeScript + Zustand + 自寫 CSS，對應 SPEC §3.2 與附錄 D 的 **M7**。
API 契約以 [`docs/API.md`](./API.md) 為準（型別已對齊，見 §3）。

---

## 1. 執行方式

```bash
cd web
npm install

# A. 真後端（需先啟動 daemon）
cargo run -- serve        # 另一個終端，daemon 監聽 127.0.0.1:7788
npm run dev               # http://localhost:5173

# B. Mock 模式（不需要 daemon、不需要 herdr）
VITE_MOCK=1 npm run dev

# 建置（M8 由 rust-embed 內嵌 web/dist）
npm run build             # tsc -b && vite build → web/dist
npm run lint              # oxlint
```

`vite.config.ts` 的 proxy 把 `/api`、`/hook` 轉 http、`/ws` 轉 ws 到 `127.0.0.1:7788`。
**`changeOrigin: true` 是必要的**：daemon 會檢查 `Host` 必須是 `127.0.0.1:<port>` /
`localhost:<port>`（API.md §0），不改寫 Host 會拿到 403。
要指向別的 port 用環境變數：`VITE_DAEMON=http://127.0.0.1:9999 npm run dev`。

---

## 2. 檔案結構

```
web/src/
  api/
    types.ts       # SPEC §7 / API.md 的 TypeScript 型別（單一集中處）
    normalize.ts   # 寬鬆解碼：吸收 0/1 vs bool、args vs args_json、巢狀 vs 扁平等差異
    transport.ts   # Transport 介面 + 真後端實作（fetch + WebSocket 自動重連）
    mock.ts        # 記憶體假後端（VITE_MOCK=1）
    index.ts       # 依 VITE_MOCK 選 transport，對外只暴露具名 API 函式
  store/store.ts   # 單一 Zustand store：server state 鏡像 + UI state + WS 事件處理
  components/
    Sidebar.tsx    # Project 分組、狀態燈、start/stop、新增 Project / Bot 表單
    ChatPanel.tsx  # 標題列、對話/終端分頁、氣泡列表、輸入框
    BotSettingsPanel.tsx # Bot 設定（改名 / 模型 / args / 身份 / env / 刪除），另 export ModelField
    BlockedPanel.tsx # blocked 時的終端快照 + 按鍵面板
    TerminalTab.tsx  # recent_unwrapped 唯讀快照 + 刷新
    StatusLamp.tsx   # §2.2 合成燈號
  styles.css       # 全部 CSS（淺色在 :root，深色在 prefers-color-scheme）
  App.tsx / main.tsx
```

設計原則：**所有跟後端 JSON 形狀有關的知識都集中在 `api/`**。元件與 store 只看
`types.ts` 的型別，之後 API 有變動只要改 `normalize.ts`。

---

## 3. 與 `docs/API.md` 的對齊狀況

型別是先依 SPEC §7 寫好，`docs/API.md` 出現後逐項核對過，目前**完全一致**：

| 項目 | 對齊方式 |
|---|---|
| `GET /api/session` → `{token, port}` | `transport.ts` 取 `token`，之後 REST 帶 `X-AM-Token`、WS 帶 `?token=` |
| `GET /api/state` 巢狀 `projects[].bots[].run` | `normalize.toState()` 攤平成 `projects / bots / runs` |
| `autostart` / `inject_hooks` bool、`adopted` 0/1 | `normalize.bool()` 同時吃 bool 與整數 |
| 錯誤 body `{error, reason\|message\|what}` | `ApiError` 依 `reason → message → what → error` 取人類可讀訊息 |
| `POST /prompt` → `{turn_id, message_id, delivery}` | `delivery=unknown` 會跳警示並提供「放棄該回合」 |
| `GET /messages` → `{messages(正序), turns, has_more}` | `turns` 用來判斷 in-flight / delivery 警示 |
| `GET /terminal` → `{text, revision, truncated, agent_status}` | blocked 面板每秒輪詢 `visible`、終端分頁手動刷新 `recent_unwrapped` |
| WS `{seq, type, data}`、`resync` | 記錄最高 `seq`，重連帶 `?since=`；`resync` → 重新 `GET /state` + 目前 bot 的 messages |
| keys 鍵名 `enter/esc/y/n/up/down/ctrl+c` | 按鍵面板使用同一組字串，並帶 `expect_run_id` |

**一處刻意的差異**：API.md §6 建議 `source = "hook"` 不加標籤。本 UI 依 SPEC §3.2
（「assistant 氣泡顯示來源標籤（hook / terminal-fallback）」）兩種都顯示標籤，
`terminal_fallback` 另外加「可能不完整」黃字。

**`lamp` 欄位**：`GET /api/state` 會回後端算好的 `lamp`，但前端**自己重算**
（`normalize.lampOf(run, connected)`），因為 `bot_status` WS 事件只帶 `run` 不帶 `lamp`，
若採用後端值會在事件更新後失準。兩邊的規則相同（SPEC §2.2）。

---

## 4. UI 行為重點

### 狀態燈（SPEC §2.2）

| lamp | 呈現 |
|---|---|
| `disconnected` | 灰點 |
| `offline` | 空心灰圈 |
| `starting` | 黃點閃爍 |
| `stopping` | 黃點 |
| `idle` | 綠點 |
| `working` | 藍點 + 外圈脈動動畫 |
| `blocked` | 紅點 |
| `unknown` | 灰黃點 |

`prefers-reduced-motion: reduce` 時所有動畫關閉。

### 輸入框鎖定（SPEC §6.3）

`composerState()` 依序判斷並顯示原因：

1. 未選 Bot
2. daemon 與 herdr 連線中斷
3. 無 active Run（「請先按啟動」）
4. Run 不在 `running`
5. `agent_status = blocked`（引導到上方按鍵面板）
6. 有 `delivery = unknown` 的 Turn → 附「放棄該回合」按鈕（`POST /turns/:id/abandon`）
7. 有 `in_flight` Turn → 附「中斷（esc）」按鈕

送出時前端產生 `client_request_id`（`crypto.randomUUID()`，非安全來源時退回自製 v4），
作為冪等鍵。使用者氣泡**不做本地暫存**，一律等 `message_added` 推回來（依 `message.id` 去重），
符合 API.md §9 的建議。後端回 409 時，`reason` 直接顯示在右下角通知。

輸入框：Enter 送出、Shift+Enter 換行，並排除輸入法組字中的 Enter（`isComposing`）。

`POST /prompt` 回 `delivery = "failed"`（例如 agent 正 blocked）時**不塞本地的假 Turn**
（REVIEW B10）：直接跳通知並 `loadMessages`，輸入框保持可用、文字留在框裡讓使用者重送。
只有 `pending` / `ok` / `unknown` 才會先在 `turns` map 補一筆，讓輸入框在 WS 事件抵達前就鎖住。

### blocked 面板

`agent_status = blocked` 時，在聊天視窗上方展開：等寬字體的 `visible` 快照（每 1 秒輪詢
`GET /api/bots/:id/terminal?source=visible&lines=40`），下方是
`Enter / Esc / y / n / ↑ / ↓ / ctrl+c` 按鍵按鈕，呼叫 `POST /api/bots/:id/keys` 並帶
`expect_run_id`。

### 終端分頁

右上「對話 / 終端」切換。終端分頁是 `recent_unwrapped` 的唯讀快照，可選 50/100/200/500 行，
手動「刷新」。不做 xterm.js（SPEC §9 非目標）。

### WebSocket

`transport.ts` 內建指數退避重連（上限 10 秒 + jitter）。重連時帶 `?since=<最高 seq>`；
收到 `resync` 就重新 `GET /api/state` 並重載目前 bot 的訊息（有 in-flight 保護避免重入）。
`project_changed` / `bot_changed` 也會觸發重新 `GET /api/state`。

### 主題與版面

深淺色跟隨系統（`prefers-color-scheme`），不提供手動切換。
版面在 1280 與 900 寬都測過；≤1080 會收起 sidebar 的連線文字與標題列的 run/pane 細節，
≤780 sidebar 變成可開合的抽屜（左上 ☰）。

---

## 5. Mock 模式（`VITE_MOCK=1`）

`api/mock.ts` 是一個記憶體假 daemon，實作 API.md 的全部端點與 WS 事件（回應形狀刻意
和 `daemon/src/api.rs` 一致），時間軸壓縮：start 約 1.4 秒就緒、回覆約 1.9 秒。
左上角會顯示 `MOCK` 徽章。預設種了一個 project（`agents-manager`）與兩個 bot。

### 用訊息內容觸發不同情境

| 訊息包含 | 效果 |
|---|---|
| `blocked` 或 `rm -rf` | agent 進入 `blocked`，終端快照顯示確認框；按 `y` / `Enter` 繼續，`n` / `Esc` / `ctrl+c` 取消 |
| `fallback` | 回覆以 `source = terminal_fallback`、`incomplete = 1` 進來（標「可能不完整」） |
| `slow` | 回覆延遲約 8 秒，方便觀察輸入框鎖定與 typing 指示 |
| 其他 | 約 1.9 秒後以 `source = hook` 回覆 |

### 瀏覽器 console 的除錯開關

```js
__amMock.dropSocket()   // 斷線 1.5 秒後重連並送 resync
__amMock.resync()       // 直接送一則 resync
__amMock.block('demo-claude')  // 不送訊息直接讓某個 bot 進 blocked
__amMock.disconnect() / __amMock.reconnect()  // 模擬 daemon 與 herdr 斷線（燈號轉灰）
```

---

## 6. 驗收紀錄（2026-09-05）

截圖在 [`docs/screenshots/`](./screenshots/)，用 headless Chrome（CDP）自動走完流程產生。

### Mock 模式完整流程

| 檔案 | 內容 |
|---|---|
| `01-overview.png` | 初始畫面，兩個 offline bot |
| `02-new-project-form.png` / `03-new-bot-form.png` | 新增 Project（`/Users/me/project/demo-app`）與 Bot（`demo-claude`, claude, `--model opus`） |
| `04-starting.png` | start 後 `starting`（黃閃燈） |
| `05-working.png` | 送出訊息，`working`（藍動畫）+ 輸入框鎖定 + typing 指示 |
| `06-reply-hook.png` | 收到 `source = hook` 的回覆 |
| `07-reply-fallback.png` | `source = terminal_fallback` 回覆，標「可能不完整」 |
| `08-blocked.png` | blocked 面板：終端快照 + 按鍵列，輸入框顯示鎖定原因 |
| `09-after-keys.png` | 按 `y` 之後恢復並收到回覆 |
| `10-terminal-tab.png` | 終端分頁（`recent_unwrapped` + 刷新） |
| `10b-socket-reconnecting.png` / `10c-after-resync.png` | 斷線 → 重連 → `resync` 後訊息數不變（7 → 7） |
| `11-dark.png` | 深色主題 |
| `12-narrow-900.png` | 900 寬版面 |
| `13-stopped.png` | stop 之後回到 offline |

### 真後端（`agents-managerd serve` @ 127.0.0.1:7788）

| 檔案 | 內容 |
|---|---|
| `20-real-daemon-overview.png` | 真 state：`agents-manager` project 下的 `am-claude` / `am-codex`，皆為綠燈（running + idle），WS `open` / herdr `connected` |
| `21-real-daemon-terminal.png` | 真 pane 的 `recent_unwrapped`（可看到 Claude Code v2.1.261 的畫面） |
| `22-real-daemon-chat.png` | 切換 bot 後重新載入該 bot 的對話 |

真後端這一輪**刻意只做讀取**（state / messages / terminal / WS / 切換 bot），沒有
start / stop / prompt，因為當時後端 agent 正在同一個 daemon 上跑 M4 / M5 的驗證，
不想干擾他的 in-flight Turn 與 fixtures。寫入路徑（新增、start/stop、prompt、keys、
abandon）已在 mock 模式完整走過，且請求形狀與 `daemon/src/api.rs` 逐一核對過。

驗證結果：`GET /api/session` → `GET /api/state` → `/ws` 全部正常，載入 20 則訊息，
`terminal_fallback` 訊息正確標示，主控台無任何錯誤或例外。

---

## 7. 已知問題與待辦

1. **真後端的寫入路徑尚未在瀏覽器實跑**（原因見 §6）。等後端 M4/M5 驗證告一段落，
   應再用真 daemon 走一次「新增 → start → prompt → blocked → keys → stop」。
2. **訊息分頁未實作**。目前 `GET /messages?limit=200` 一次抓完，`has_more` 與 `before`
   參數已在型別與 `normalize` 內備妥，但 UI 還沒有「載入更早訊息」按鈕。
   對話很長時會全部塞在 DOM 裡。
3. **未讀計數固定 0**（SPEC §9 第一階段非目標），`unread` 欄位讀進來但不顯示。
4. **`turn_updated` 的 in-flight 判斷依賴事件抵達**。若 WS 在 prompt 與 hook 之間斷線且
   `resync` 沒補上，輸入框可能短暫誤判為可用；再送會被後端 409 擋下並顯示原因，
   不會造成錯配，但體驗上會多一次失敗。
5. ~~**`PATCH /api/bots/:id` 尚未接 UI**~~ → 2026-09-06 已完成，見下方「Bot 設定面板」。
6. **`inject_hooks` / `args` / `env` 完全不在 UI 上**（後端支援，`inject_hooks` 預設 true）。
   使用者決定不顯示，要改只能編 `config.toml`。
7. **終端快照是純文字**，ANSI 已由後端去除，但 box-drawing 字元在含中文的行會對不齊
   （等寬字體對 CJK 的寬度處理）。不影響操作。
8. **`terminal_fallback` 的長訊息**會整段終端內容進氣泡；超過 ~900 字或 18 行的氣泡預設
   收合並提供「展開全文」。
9. **oxlint 兩個 warning** 未修：`StatusLamp.tsx` 同時 export 常數（fast-refresh 提示）、
   `TerminalTab.tsx` 的 set-state-in-effect（非同步刷新，誤報）。不影響 build。

---

## 8. 給後端 / M8 的注意事項

- `npm run build` 產物在 `web/dist`，入口 `dist/index.html`，資產路徑是絕對的 `/assets/…`，
  rust-embed 掛在 root fallback 即可（`daemon/src/assets.rs` 已經是這個形狀）。
- 前端只在啟動時打一次 `GET /api/session`；token 存在記憶體，不寫 localStorage。
- 前端**不會**呼叫 `/hook/*`，proxy 設定只是為了本機除錯方便。
- 如果 `GET /api/state` 之後要加欄位，前端會安全忽略未知欄位；但如果**改欄位名**，
  請同步更新 `web/src/api/normalize.ts`（那是唯一需要改的地方）。


## 目錄選擇器（2026-09-05 新增）

新增 Project 表單的「瀏覽…」按鈕會開啟 `DirPicker`（`web/src/components/DirPicker.tsx`），透過 `GET /api/fs/dirs` 逐層瀏覽：上一層、家目錄、麵包屑、手動輸入路徑、單擊進入子目錄、雙擊直接選取、「選擇此目錄」帶回表單並自動填 label。mock 模式有一棵假目錄樹。截圖 `docs/screenshots/40-42`。


## 遠端主機（SPEC §11.6，2026-09-06 新增）

Project 可以位於另一台機器（daemon 透過 SSH 轉發連遠端 herdr）。前端把「本機」當成一個
保留的 host `local`，其餘都是使用者設定的遠端主機。

### 資料流

| 來源 | 欄位 | 前端處理 |
|---|---|---|
| `GET /api/state` | `hosts[]`（**第一筆固定是 `local`**，ssh 欄位為 null） | `normalize.toState()` 濾掉 `local`，store 的 `hosts` 只留遠端；本機狀態仍看頂層 `connected` |
| `GET /api/state` | `projects[].host`（`"local"` 或 host name） | `Project.host`；舊 daemon 沒有這個欄位時預設 `"local"` |
| WS `daemon_status` | `{herdr_connected, connected, hosts: {name: {connected, error}}}` | 讀 `herdr_connected`（退回舊的 `connected`），`hosts` map 以 `mergeHosts()` 併入既有 host 設定 |
| WS `host_changed` | `{name, connected, error}` | 直接更新該 host；`name = "local"` 時改的是 `connected`；沒見過的名字（新增／刪除通知）觸發 `GET /api/state` |
| `POST /api/hosts` | `{name, connected, error}` | 表單下方顯示連線結果（綠框 / 紅框），同時跳右下角通知 |

**燈號**：`botLamp()` 先看 bot 所屬 Project 的 host。遠端 host `connected = false` →
該 host 底下所有 bot 一律 `disconnected`（灰），不看 run 狀態；本機 bot 沿用頂層 `connected`。
`composerState()` 對應顯示鎖定原因「主機未連線（`<name>`）：`<error>`」。

**相容性**：daemon 若還沒實作 §11（`/api/state` 沒有 `hosts`），前端會得到空的 host 清單、
所有 project `host = "local"`，行為與 §11 之前完全相同（已對現行 daemon 實測，見下方截圖 `6c`）。

### UI

- **sidebar 底部「主機」disclosure**（`components/HostsPanel.tsx`）：右側顯示 `本機 + N ・ M 個未連線`。
  每個 host 一列：連線燈（綠 / 灰）、名稱、使用中的 Project 數、`ssh 目標:port ・ session`、
  未連線時的錯誤字串、「重連」（`POST /hosts/:name/reconnect`）與「✕」刪除
  （`DELETE /hosts/:name`；仍有 Project 使用會被後端 409 擋下並顯示 reason）。
- **新增主機表單**：`名稱`、`ssh 目標` 兩個必填欄位；`ssh_port` / `herdr_session` /
  `remote_path` / `hook_port` / `ssh_opts` 收在「進階（都有預設值）」裡，預設值取自 SPEC §11.2
  （`22` / `agents-manager` / `/opt/homebrew/bin:$HOME/.local/bin` / `7788`）。
  送出後按鈕變「連線中…」（後端最多約 20 秒），結果直接顯示在表單上方。
- **新增 Project 表單**多一個「主機」下拉（本機 + 已設定 hosts，未連線的 host 標示並停用）；
  切換主機會清掉已選路徑，`DirPicker` 帶 `host` 呼叫 `GET /api/fs/dirs?host=…`，
  並在頂端顯示 `@<host> 遠端目錄`。送出時帶 `host`。
- **host 徽章** `@<host>`：sidebar 的 Project 標題與右側標題列的 bot 名稱旁邊；本機不顯示。
  host 斷線時徽章轉灰並加刪除線。

### mock 模式的主機（`VITE_MOCK=1`）

`api/mock.ts` 完整實作了 `/hosts` 三個端點、`?host=` 的遠端目錄樹、`hosts[]`（含 `local`）
與 `host_changed` / `daemon_status` 事件。假 ssh 撥號規則：**ssh 目標含 `fail` / `bad` /
`unreachable` / `0.0.0.0` 會連線失敗**（回 `connected:false` + 錯誤字串），其餘成功。
遠端目錄樹以 ssh 目標的使用者名稱為家目錄（`m4p@…` → `/Users/m4p`，底下有
`work/{api-server,web-client,scratch}`、`src/herdr`）。

console 開關（在既有的 `__amMock` 上）：

```js
__amMock.hosts()          // 目前設定的 host 名稱
__amMock.hostDown('m4p')  // 模擬 ssh master 掛掉：燈號轉灰、composer 鎖定
__amMock.hostUp('m4p')    // 恢復
```

### 驗收紀錄（2026-09-06，mock）

`node scripts/demo-hosts.mjs`（headless Chrome / CDP，先 `VITE_MOCK=1 npx vite --port 5199`）：

| 檔案 | 內容 |
|---|---|
| `60-hosts-empty.png` | 「主機」disclosure 展開，尚未設定遠端主機 |
| `61-new-host-form.png` | 新增主機表單（含展開的進階欄位） |
| `62-host-connected.png` | 新增 `m4p`（`m4p@100.112.229.82`）→ 綠燈 + 「ssh master 與遠端 herdr 都就緒」 |
| `63-remote-dirpicker.png` / `64-remote-dirpicker-work.png` | 選擇器切到 `@m4p`，列出 `/Users/m4p`、`/Users/m4p/work` |
| `65-remote-project.png` | 新增 Project `api-server@m4p`，標題列出現 `@m4p` 徽章 |
| `66-remote-bot-running.png` | 遠端 bot `api-claude` 啟動後綠燈，標題列顯示徽章 |
| `67-host-down.png` | `__amMock.hostDown('m4p')` → 燈號轉灰、徽章刪除線、composer 鎖定「主機未連線」 |
| `68-host-down-panel.png` | 主機面板顯示錯誤字串與「重連」 |
| `69-host-reconnected.png` | 重連後燈號恢復、composer 解鎖 |
| `6a-dark.png` / `6b-narrow-900.png` | 深色主題、900 寬 |
| `6c-real-daemon-no-hosts.png` | 相容性檢查：對**還沒有 `hosts` 欄位的 daemon**，無徽章、本機燈號不變、主機面板為空狀態，console 無錯誤 |

### 驗收紀錄（2026-09-06，真後端 `agents-managerd serve` @ 127.0.0.1:7788）

daemon 端 §11 上線後用 `node scripts/demo-hosts-real.mjs`（前端 `npx vite --port 5198`，proxy 到
7788）走過。**刻意不建立 Project、不 start 遠端 bot**（後端 agent 正在同一個 daemon 上驗收 R1–R3）。

| 檔案 | 內容 |
|---|---|
| `6c-real-host-connected.png` | 從 UI 新增主機 `m4p`（`m4p@100.112.229.82`）→ 寫入成功、連線失敗，紅框顯示 daemon 回的錯誤字串；主機列出現 `loop`（後端 R5 的測試 sshd）與 `m4p`，disclosure 顯示「本機 + 2 ・ 2 個未連線」 |
| `6d-real-remote-dirpicker.png` | 新增 Project 表單選 `m4p` → 選擇器標示 `@m4p 遠端目錄`，列出**真實的** `/Users/m4p`（Applications / Desktop / Documents / go / project …），確認 `GET /api/fs/dirs?host=` 端到端可用 |
| `6e-real-remote-dirpicker-sub.png` | 進入子目錄 |

這一輪在真後端上發現兩個 **daemon 端**問題（已回報，不在前端範圍）：

1. `POST /api/hosts` 與 `reconnect` 一律回 `connected:false`，`error` 為
   `ssh <target> failed (exit status: 1): zsh:5: bad output format specification`
   ——遠端 sh 片段被遠端登入 shell（zsh）解讀，`loop` 與 `m4p` 都一樣。
2. `GET /api/fs/dirs?host=` 的第一筆是名稱為 `'` 的假目錄（進去會變成 `/Users/m4p/'`），
   遠端列目錄的 sh 片段有引號外漏。另外 host `connected = false` 時這個端點仍會成功回傳，
   與 API.md 寫的「host 斷線 → 502」不一致。

前端對兩者的呈現都正確（錯誤字串顯示在主機列與通知、目錄照回傳內容渲染）。

### 已知問題（遠端主機）

1. **真後端上還沒走完「建立遠端 Project → start 遠端 bot → 對話」**。上面那一輪只做到
   新增主機與遠端目錄瀏覽（後端 agent 正在同一個 daemon 上驗收 R1–R3，避免干擾）；
   等 daemon 的 ssh 片段修好、host 能連上之後要再跑一次完整流程。
   目前 `config.toml` 裡留了一個由 UI 新增的 host `m4p`（連線失敗狀態）。
2. **`POST /api/hosts` 最長約 20 秒**才回應（ensure session + ssh master + ping），期間表單
   只顯示「連線中…」，沒有進度或取消。
3. **host 設定不能編輯**：改 ssh 目標要重新送一次同名的「新增主機」（後端視為更新），
   UI 沒有「編輯」入口。
4. **`local` 不出現在主機清單**：本機的連線狀態只在左上角的連線徽章顯示，主機面板不列它，
   因此也沒有「重新 ping 本機 herdr」的按鈕（後端的 `POST /hosts/local/reconnect` 沒接 UI）。
5. **遠端 Project 的路徑沒有前端驗證**：路徑不存在由後端 ssh 檢查後回 400。

## 身份（identities，2026-09-06 新增）

sidebar 底部「身份」面板列出 `identities[]`（名稱、kind、env 摘要、使用中的 Bot 數）並可新增 / 刪除。新增身份表單只有 **名稱 / kind / env**（env 就是身份的本體，例如 `CLAUDE_CONFIG_DIR`；`args` 契約仍在但 UI 不再提供，2026-09-06）。新增 Bot 表單有「身份」下拉（只列同 kind 的身份）；bot 列在 kind 標籤旁顯示身份徽章。契約見 `docs/API.md` 的 identities 章節（bot.identity、bot.env、`POST /api/identities`、`DELETE /api/identities/:name`、WS `identities_changed`）。mock 內建 `cc0`（無 env）與 `cc1`（`CLAUDE_CONFIG_DIR=$HOME/.claude-ccompany`）。驗收腳本 `scripts/demo-identity.mjs`，截圖 `docs/screenshots/80-82`。

## Bot 設定面板（API.md §10 / v3.3，2026-09-06 新增）

改名、改模型、改身份、autostart / auto_approve、刪除 bot、重啟套用。
契約以 [`docs/API.md` §10](./API.md) 為準。

> **UI 刻意不提供 `args` / `env` / `inject_hooks`**（2026-09-06 使用者決定）。
> 型別與 API 呼叫都保留這三個欄位，但新增 Bot 不帶、PATCH 也不送，
> 所以 `config.toml` 既有的值不會被 UI 動到。要調參數請用**身份（identity）**或直接改
> `config.toml`。

### 入口

- **bot 列的齒輪** ⚙（`.icon-btn.gear`，hover / 選取時整排 `.bot-actions` 才浮出）。
- **右側標題列** bot 名稱旁的齒輪（`aria-expanded` 反映開合）。

兩者都呼叫 `openSettings(botId)`：順便把選取切到該 bot，面板**覆蓋在對話 / 終端之上**
（標題列的燈號、run/pane、啟動 / 停止仍在），按 ✕、「關閉」或切「對話 / 終端」分頁即回到對話。

### 欄位與送出

| 欄位 | 備註 |
|---|---|
| 名稱 | 有 active Run 時 `disabled`，下方提示「停止後才能改名」＋「停止並改名」按鈕（stop → 輪詢到 run 進 `stopped` / 消失 → 自動 focus 回輸入框）。前端仍檢查 `[a-z][a-z0-9_-]{0,31}` |
| kind | 唯讀 |
| **模型** | `<select>`：`（預設）`＋常用別名＋`自訂…`。claude = `opus` / `sonnet` / `haiku`，codex = `gpt-5.5` / `gpt-5.6-luna` / `gpt-6-astra`。選「自訂…」多出一個文字框，可送任意字串；清空 = `null` |
| args | 空白分隔 |
| 身份 | 只列同 kind 的 identities；`（無）` = `null` |
| env | 每行 `KEY=VALUE`（與身份面板共用 `parseEnvText` / `envToText`），整包取代 |
| autostart / auto_approve / inject_hooks | checkbox |

`ModelField` 由 `components/BotSettingsPanel.tsx` export，**新增 Bot 表單也用同一個**
（預設「（預設）」，claude 的提示字：「claude 預設可能是 haiku，建議選 opus 或 sonnet」）。
`api/types.ts` 的 `MODEL_OPTIONS` 是唯一的清單來源。

**只送有變更的欄位**：面板每次 render 把表單狀態和 `bot` 逐欄比對成一個 `PatchBotInput`，
按鈕旁顯示「已變更：model, identity」；沒有變更時「儲存」是 disabled。
比對範圍只有面板上真的顯示的欄位，所以 `args` / `env` / `inject_hooks` 永遠不會進 body。

### needs_restart

`PATCH` 回 `{needs_restart:true}`（有 active Run 且動到影響啟動 argv/env 的欄位）時，面板頂部
出現黃色 `.bs-banner.warn`「已儲存，重啟 Bot 後生效」＋「立即重啟」→ `POST /api/bots/:id/restart`，
成功後橫幅消失、燈號 `starting → idle`。回 `false`（沒有 Run，或只改了 `autostart`）則顯示
藍色「已儲存」橫幅並跳一則通知。

### 刪除

面板底部紅框「刪除 Bot」，`confirm` 文案說明「會停止並關閉它的終端 pane（有 active Run 也會先
停止），設定從 config.toml 移除，對話紀錄會保留」。刪除後選取移到**同一個 Project 的下一個
bot**（沒有就往前一個，都沒有則 `null`；`removeBot` 會蓋掉 `refreshState` 的「退回 bots[0]」）。

### mock 補齊（`VITE_MOCK=1`）

`bots[].model`、`PATCH` 回 `needs_restart`（只有 `model` / `args` / `identity` / `env` /
`auto_approve` / `inject_hooks` 算數，只改 `autostart` → `false`；UI 實際上只會送到
`model` / `identity` / `auto_approve` 這三個會觸發重啟的欄位）、`POST /bots/:id/restart`
（先把 run 收掉再 start）、`DELETE /bots/:id`（有 Run 先停、訊息保留）、改名衝突 409
（`cannot rename a bot with an active run` / `bot name already in use`）、identity kind 不符 → 400。
種子資料的 `am-claude` 改成 `model: null`（原本是 `args: ["--model","opus"]`）。

### 驗收紀錄（2026-09-06）

`node scripts/demo-botsettings.mjs`（先在 `web/` 內 `VITE_MOCK=1 npx vite --port 5183`）：

| 檔案 | 內容 |
|---|---|
| `100-bot-settings.png` | 齒輪開啟面板；`am-claude` 執行中 → 名稱欄 disabled ＋「停止並改名」 |
| `101-bot-settings-model-changed.png` | 模型改 `opus`，顯示「已變更：model」；欄位只有 名稱 / kind / 模型 / 身份 / autostart / auto_approve |
| `102-needs-restart.png` | 儲存 → 黃色「已儲存，重啟 Bot 後生效」＋「立即重啟」 |
| `103-restarting.png` / `104-restarted-idle.png` | 立即重啟 → `starting`（黃閃）→ `idle`（綠），橫幅消失，列上出現 `opus` 徽章 |
| `105-rename-after-stop.png` | 「停止並改名」→ 停止完成後名稱欄解鎖，輸入 `am-claude-fast` |
| `106-renamed.png` | 儲存 → 藍色「已儲存」，sidebar 名稱更新 |
| `107-dark.png` / `108-narrow-900.png` | 深色主題、900 寬（`.bs-danger` 轉直排） |
| `109-new-bot-model-field.png` | 新增 Bot 表單的「模型」欄位與 haiku 提示 |
| `110-delete-section.png` / `111-after-delete.png` | 刪除區塊；刪除後列表只剩 `am-codex`，選取自動移過去 |
| `116-identity-form-no-args.png` | 身份面板：新增表單只剩 名稱 / kind / env，身份列只顯示 env 摘要 |

真後端（`agents-managerd serve` @ 127.0.0.1:7788），`node scripts/demo-botsettings-real.mjs`
（前端 `npx vite --port 5184`）。**只動本機的 `am-codex`**，沒碰遠端 `@m4p` 的 bot：

| 檔案 | 內容 |
|---|---|
| `112-real-bot-settings.png` | 真 state 下開啟面板：codex 的模型選項、名稱欄因 active Run 而 disabled |
| `113-real-needs-restart.png` | 模型改 `gpt-5.5` → 儲存 → daemon 回 `needs_restart: true` |
| `114-real-restarting.png` / `115-real-restarted.png` | 立即重啟 → `starting` → `idle`，標題列與 bot 列出現 `gpt-5.5` 徽章 |

事後確認 `~/.config/agents-manager/config.toml` 寫入了 `model = "gpt-5.5"`，
`GET /api/state` 的 `am-codex.model` 也是 `gpt-5.5`。全程主控台無例外。

### 已知問題（Bot 設定）

1. **模型清單是前端寫死的**（`MODEL_OPTIONS`），後端不做白名單驗證。CLI 換代號時要改
   `web/src/api/types.ts`；在那之前使用者可以用「自訂…」輸入任意字串。
2. **UI 完全不能編輯 `args` / `env` / `inject_hooks`**（刻意的）。既有值只看得到於
   `config.toml`；bot 層級的 env 要靠身份（identity）帶。
3. **改名沒有「改完自動再啟動」**：「停止並改名」只負責停止，改完要自己按「啟動」。
4. **`needs_restart` 的判斷完全信後端**。前端不自己推論哪些欄位需要重啟。
5. **面板沒有未存變更的離開確認**：切 bot / 切分頁 / 按 ✕ 會直接丟掉未儲存的編輯。
