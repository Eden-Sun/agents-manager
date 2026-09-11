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
    types.ts       # SPEC §7 / API.md 的 TypeScript 型別（單一集中處；含 v4.0）
    normalize.ts   # 寬鬆解碼：吸收 0/1 vs bool、args vs args_json、巢狀 vs 扁平等差異
    transport.ts   # Transport 介面 + 真後端實作（fetch + WebSocket 自動重連）
    mock.ts        # 記憶體假後端（VITE_MOCK=1；含 v4.0 models/quota/tools/issues）
    mentions.ts    # SPEC §13 的 @mention 規則（與 daemon/src/group.rs 同一套；輸入框與 mock 共用）
    index.ts       # 依 VITE_MOCK 選 transport，對外只暴露具名 API 函式
  store/store.ts   # 單一 Zustand store：server state 鏡像 + UI state + WS 事件處理
  store/routeSync.ts # 網址 ↔ store 的雙向同步（pushState / popstate / document.title）
  lib/routes.ts    # `parse(pathname) → Route` / `build(Route) → pathname`（純函式，有單元測試）
  hooks/
    useTerminalSnapshot.ts # `GET /terminal` 輪詢（blocked 面板與全畫面共用，可 pause）
    usePaneKeys.ts         # 鍵名對照（KeyboardEvent → herdr）＋ 依序送鍵的佇列、KEYPAD 按鍵列
  components/
    Sidebar.tsx    # Project 分組、狀態燈、start/stop、新增 Project / Bot 表單
    ChatPanel.tsx  # 標題列、對話/終端分頁、氣泡列表、輸入框（export Bubble）
    GroupChatPanel.tsx # SPEC §13 專案群組聊天：成員燈號列、合併時間軸、@mention 自動完成
    BotSettingsPanel.tsx # Bot 設定（改名 / 模型 / args / 身份 / env / 刪除），另 export ModelField
    BlockedPanel.tsx # blocked 時對話上方的終端快照 + 按鍵面板
    BlockedModal.tsx # blocked 時自動彈出的全畫面 herdr 終端（鍵盤直通 + 按鍵列）
    TerminalTab.tsx  # recent_unwrapped 唯讀快照 + 刷新
    StatusLamp.tsx   # §2.2 合成燈號
    AttachButton.tsx # v4.0：一鍵複製 hosts[].attach_command
    QuotaStrip.tsx   # v4.0：頂欄 5h/7d 剩餘額度（掛在 Chat/Group 標題列；一次顯示一台主機，SPEC §14）
    MemBadge.tsx     # RAM 那一格（SPEC §15）；點得開，內容交給 MemPopover
    MemPopover.tsx   # RAM 展開的程序清單 + 結束/停止 bot（SPEC §15.2）
    MemPaneModal.tsx # 清單裡點「自己開的 pane」開的視窗：那個 pane 的畫面，只讀
    Tools.tsx        # v4.0：工具徽章、可收合缺 CLI 提示、用現有 agent 安裝
    KindTag.tsx      # v4.0：kind 圖示/文字全域切換（localStorage）
    ModelPicker.tsx  # v4.0：GET /api/models 驅動的模型 / effort / Fast
    IssuesBar.tsx    # v4.0：專案 github 非 null 時的 Issues 下拉（未登入時可從錯誤列登入該主機的 gh）
    GhAuth.tsx       # 遠端／本機 `GET|POST /api/hosts/:name/gh`：HostsPanel 列上的狀態 + IssuesBar 的「登入」鈕
    UpdateBadge.tsx  # claude 有新版等著重啟套用時，標題列上那顆點得下去的 chip（run.update_notice）
    UpdateQuotaChip.tsx # 額度列上的 `⬆ N`：claude 全域有新版，一次重啟所有閒置的 Bot（SPEC §6.9）
    HostsPanel.tsx / IdentitiesPanel.tsx / DirPicker.tsx
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
| keys 鍵名 `enter/esc/y/n/up/down/ctrl+c` | 按鍵面板使用同一組字串，並帶 `expect_run_id`；鍵盤直通另外送單一字元與 `ctrl+`／`alt+`／`shift+` 組合（herdr 0.8.2 實測皆收） |
| `GET\|POST /api/hosts/:name/gh` 登入遠端 gh | `GhAuth.tsx`：HostsPanel 列上的狀態；IssuesBar 在 502 未登入時出「登入」鈕。token 不會進前端 state |

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

### blocked 面板與全畫面終端

`agent_status = blocked` 時有兩層畫面，兩邊共用 `useTerminalSnapshot` 與 `usePaneKeys`：

1. **全畫面（`BlockedModal`）**：blocked 之後 1 秒（`AUTO_OPEN_DELAY_MS`）**自動彈出**，
   `visible` 200 行，每秒更新。那一秒是留給 daemon 的：claude 的滿意度問卷之類的東西
   （SPEC §3.1 `tui_prompts`）它自己會按掉，不該為了那個閃一個全畫面視窗出來。
   要決定「按 y 還是 n」得看到完整的對話框，對話上方那塊 300px 的截角不夠。
   只彈**目前正在看的**那個 bot：別的 bot 進 blocked 交給側欄紅點，不打斷手上的事。
   關掉之後不再自動彈回來，直到這個 bot 離開 blocked 又再進去一次（那是另一個問題）。
2. **面板（`BlockedPanel`）**：聊天視窗上方的 `visible` 40 行快照，全畫面關掉後的留守，
   標題列有「展開全畫面」把它叫回來。全畫面開著時面板暫停輪詢，一個 bot 只有一條
   `GET /terminal` 在跑。

兩邊都有 `Enter / Esc / y / n / ↑ / ↓ / ctrl+c` 按鍵列（`POST /api/bots/:id/keys`，帶
`expect_run_id`）。全畫面另有**鍵盤直通**（預設開）：`herdrKeyFromEvent` 把 `KeyboardEvent`
翻成 herdr 鍵名直接送進 pane——具名鍵 `enter/esc/tab/backspace/up/down/left/right/f1…f12`、
任何單一字元（空白寫成 `space`）、`ctrl+` / `alt+` / `shift+` 組合。⌘ 系列留給瀏覽器（⌘C 要能
複製終端上的錯誤訊息），`home / end / pageup / pagedown / delete` herdr 不收，留著捲畫面。
直通開著時 **Esc 也會送給 agent**，關閉只走 ✕ 或點視窗外；這句話就寫在視窗頁尾。

送鍵走一條佇列：正在送的時候按下的鍵先累積，下一輪一次送出（`agent.send_keys` 吃陣列，
順序由它保證）。一顆鍵一個請求的話，打字快一點就會亂序。

### 終端分頁

右上「對話 / 終端」切換。終端分頁是 `recent_unwrapped` 的唯讀快照，可選 50/100/200/500 行，
手動「刷新」。不做 xterm.js（SPEC §9 非目標）。

### WebSocket

`transport.ts` 內建指數退避重連（上限 10 秒 + jitter）。重連時帶 `?since=<最高 seq>`；
收到 `resync` 就重新 `GET /api/state` 並重載目前 bot 的訊息（有 in-flight 保護避免重入）。
`project_changed` / `bot_changed` 也會觸發重新 `GET /api/state`。

### 主題與版面

深淺色跟隨系統（`prefers-color-scheme`），不提供手動切換。
版面在 1280 與 900 寬都測過；≤1080 會收起 sidebar 的連線文字與標題列的狀態字
（run / pane id 已改為狀態字的 tooltip），≤780 sidebar 變成可開合的抽屜（左上 ☰）。
視覺規格見文末「UI 優化（2026-09-06）」。

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
| `340-blocked-modal.png` / `341-blocked-panel-after-close.png` / `342-blocked-modal-dark.png` | blocked 自動彈出的全畫面終端、關掉後的留守面板、深色（`scripts/demo-blocked.mjs`） |
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
8. **`terminal_fallback` 的長訊息**會整段終端內容進氣泡。2026-09-06 起不再預先收合
   （使用者要求一律完整顯示，靠 `.msg-list` 捲動）。
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

新增 Project 表單的「瀏覽…」按鈕會開啟 `DirPicker`（`web/src/components/DirPicker.tsx`），透過 `GET /api/fs/dirs` 逐層瀏覽。互動照 macOS 開檔面板：

- **單擊列 = 選取**（highlight），主按鈕跟著變成「選擇「foo」」，深層資料夾不必先進去就能選。
- **進入下一層**：雙擊該列、按列右側的 `›`、鍵盤 `Enter` 或 `→`。
- **上一層**：`↑` 按鈕、麵包屑、鍵盤 `←` 或 `Backspace`；回上層時會自動把剛離開的那層 highlight 起來。
- **麵包屑**：家目錄折成 `⌂`，過長路徑橫向捲動不換行；右側 `✎` 切成手動輸入路徑（`Esc` 取回麵包屑）。
- **篩選框**（自動 focus）：即時過濾這一層，`↑↓` 移動 highlight、`⌘Enter` 直接選取 highlight、`Esc` 先清篩選再取消。
- **「隱藏資料夾」勾選框**：打 `hidden=1`，把 `.claude`、`.config` 這類目錄一起列出。
- **版面**：選擇器用 flex 撐滿整個 popup——清單 `flex:1` 吃掉剩下的高度（可點範圍盡量大），只有清單自己捲，麵包屑與底部按鈕永遠釘在畫面上；長檔名一律 ellipsis，清單 `overflow-x: hidden`，不會有橫向捲動蓋掉點擊區。帶選擇器的 popup 由 `.modal:has(.dirpicker)` 固定成 660×760（`--modal-w` CSS 變數讓 CSS 蓋得過元件傳入的寬度）。
- 底部固定顯示「選擇 <完整路徑>」，帶回表單時自動填 label。mock 模式有一棵假目錄樹（含隱藏目錄，`Downloads` 底下有 24 個項目可以測捲動與篩選）。

驅動腳本 `scripts/demo-picker2.mjs`（先 `VITE_MOCK=1 npx vite --port 5307`），截圖裁到側邊欄：

| 檔案 | 內容 |
|---|---|
| `310-picker2-home.png` | 開啟時停在家目錄，麵包屑 `⌂`，篩選框已 focus |
| `311-picker2-selected-row.png` | 單擊 `project` → 該列 highlight，主按鈕變「選擇「project」」 |
| `312-picker2-filter.png` | 進到 `⌂/project` 後輸入 `age` → 只剩 `agents-manager`（帶 `git` 標記）且自動 highlight |
| `313-picker2-keyboard.png` | 篩選框內 `↑↓` 移動 highlight |
| `314-picker2-hidden.png` | 勾「隱藏資料夾」→ 多出 `.claude` / `.config` |
| `315-picker2-picked.png` | 「選擇「project」」帶回表單，label 自動填 `project` |
| `316-picker2-dark.png` | 深色主題；同時驗證再次開啟時會停在上次選的路徑 |
| `317-picker2-long-short-window.png` | 560px 矮視窗 + 24 個項目：清單撐滿剩餘高度並自己捲（`scrollHeight>clientHeight`、無橫向捲動），底部按鈕仍在畫面內 |

舊版截圖 `40-42` 保留為對照。


## 左上角：標題、pane 數與 RAM

側邊欄標題列現在是 `AG Man ｜ pane 2 ｜ RAM 1.5G ｜ ● 已連線`。

- **標題縮寫成 `AG Man`**（全名留在 `title`）：位置讓給右邊那排徽章。CSS 上它是 `flex: none`
  ——原本的 `flex: 1` 會先把它截成「A…」；把空白吃掉的是它後面第一個徽章的 `margin-left: auto`。
- **`pane N`**（`.pane-badge`）：現在開著幾個 herdr pane，也就是有幾個 bot 的終端還在
  （run 是 `starting` / `running` / `stopping` 且已經拿到 `pane_id`；`stopped` / `exited` 早就把
  pane 還回去了，不算）。tooltip 依主機拆開（`本機：2 個`），因為 pane 是開在各自的 herdr 上。
  一個都沒有時整格不出現。
  - 實作上它跟 `botQuotaWarning` 踩同一個坑：selector **不能**自己組陣列回傳，否則
    `useSyncExternalStore` 每次都判斷「快照變了」→ 無限重渲染（React #185）。所以 selector 只取
    `bots` / `runs` / `projects` 三個穩定引用，統計放 `useMemo`。
  - 驗收 `node scripts/demo-panebadge.mjs`（截圖 `354`–`356`，最後一張把 MOCK 徽章拿掉，確認
    正式模式下這一列不會擠爆：287px 內全放得下）。

### RAM 總量（SPEC §15，2026-09-06 加）

`RAM 1.5G` 是**所有 herdr 進程樹**現在吃掉的
常駐記憶體——herdr 自己 ＋ 它底下的 pane 與 agent CLI，所有主機加總。

- 資料來自 `GET /api/mem` 與 WS `mem_updated`（見 docs/API.md），store 存在 `mem`。
- tooltip 拆給你看：`herdr 本身 X · 底下的 agent Y` / `N 個 process` / 每一台主機各多少 /
  「每 15 秒更新一次」。
- **有主機量不到時數字旁邊標 `*`**（`.mem-badge.partial`），tooltip 寫那台為什麼量不到。
  總和悄悄變小比沒有數字更糟，所以寧可標示不完整。
- 舊 daemon 沒有 `/api/mem` → `mem` 是 null → 整格不出現（不顯示假的 0）。
- 數字用 tabular-nums 等寬，跳動時不會把旁邊的東西推來推去。

mock 依「執行中的 bot 各吃一份」推算（claude 820M / codex 640M / grok 410M ＋ herdr 48M/台），
所以啟動、停止 bot 與主機斷線都真的會讓上面那格動。驗收 `node scripts/demo-membadge.mjs`：

```
idle            RAM 48M    herdr 本身 48M · 底下的 agent 0     1 個 process
one bot up      RAM 868M   herdr 本身 48M · 底下的 agent 820M  3 個 process
two bots up     RAM 1.5G   herdr 本身 48M · 底下的 agent 1.4G  5 個 process
host added      RAM 1.5G   local：1.5G ／ m4p：48M
that host down  RAM 1.5G*  m4p：量不到（未連線）              partial=true
```

截圖 `400-membadge-idle.png`、`401-membadge-running.png`、`402-membadge-partial.png`。

> 沒有在真機上核對過數字：這個沙箱裡的 `ps` 只看得到自己的 31 個 process（連我自己起的
> vite 都看不到），也沒有跑著的 herdr。演算法有 4 個單元測試（`daemon/src/memstat.rs`）；
> daemon 本身不在沙箱裡跑，`ps` 會看到完整的樹。**要注意的一個假設**：這裡算的是
> 「herdr 的子孫」，如果 herdr 是把 pane 丟給 init 領養（double fork）而不是自己當父程序，
> agent 那一半會是 0——真機上看到 `agents 0` 但明明有 bot 在跑，就是踩到這個。


## 版面巡檢與兩個修正（2026-09-06）

`node scripts/audit-pages.mjs`（先 `cd web && VITE_MOCK=1 npx vite --port 5411 --strictPort`；
`W=1280` 可換寬度）會把每個畫面走一遍——bot 對話 / bot 設定 / 終端分頁 / 群組 / 組隊 /
team 面板 / 三個 popup——對每個可見元素做三種機械檢查：

1. **相鄰兄弟互相重疊**（就是 team 成員列出事的那種），
2. **子元素畫到不會裁切也不會捲動的父容器外面**，
3. **任何超出視窗右緣的東西**，外加整份文件有沒有橫向捲動。

（`svg` 內部的 path 會互相重疊是正常的，所以整棵 svg 子樹排除。）

這一輪抓到並修掉的：

- **team 成員列擠成一團**（使用者回報：`pm dev- dev- dev- dev-4 rev` 疊在一起）。
  成因是我上一輪把 `.member` 改成群組用的 26px 圓形圖示，但 TeamPanel 的成員籤**共用同一個
  class**，於是 `pm` / `dev-1` / `rev` 這些短名被塞進 26px 的圓裡。
  修法：`.member` 還原成原本的膠囊，群組那版改用 `.member.member-icon`——
  team 那邊短名本身就是角色徽章（SPEC-team §7.3），本來就不能只剩圖示。
  實測 6 個成員（pm + dev-1…4 + rev）在 1600 與 1100 兩個寬度下相鄰重疊都是 0。
- **組隊頁「issue 全文」的 disclosure 凸出卡片 13px**：`.disclosure` 是 `width: 100%`，
  `.team-issue` 又給了左右 14px margin，加起來就超出去。改成 `width: auto`。

修完九個畫面在 1600 / 1280 兩個寬度下全部 clean。截圖 `380-audit-bot-chat.png`、
`381-audit-group.png`、`382-audit-team.png`、`383-team-members-fixed.png`。

## Bot 改名：點標題就改（2026-09-06 加）

暱稱是拿來分辨兩個 claude 的，改的頻率高，本來卻只能進設定面板改（三次點擊）。
現在標題列的名字本身就是欄位（`BotNameField`）：

- 點一下 → 變成輸入框，內容全選；`Enter` 存檔、`Esc` 取消、失焦等同 `Enter`。
- 驗證跟設定面板同一條規則：1–32 字、不能有空白或 `@ , : ;`（CJK 可以）。
  不合法時邊框轉紅，按 `Enter` 只會還原，不會送出。
- 走 `PATCH /bots/:id {name}`，**run 執行中也能改**（docs/API.md：herdr 的 agent 名稱是從
  bot id 推出來的，不受暱稱影響）。改完側邊欄同步更新。
- 別處（設定面板、另一個分頁）改了名字時，只要沒有正在編輯就會蓋掉本地草稿。
- **側邊欄也能改**（`variant="row"`）：未選取的那一列，第一下還是「開啟這個 bot」；
  **已選取**的那一列，點名字就進編輯——就是檔案總管那種「點一下、再點一下改名」。
  兩種意思因此不會搶同一個手勢，而不是二選一。取消選取時若還開著輸入框會自動收掉。
- 元件在 `components/BotNameField.tsx`，標題列用 `variant="head"`（點一下直接改），
  側邊欄用 `variant="row"` + `armed={selected}`。側邊欄那版**不是 `<button>`**：
  那一列本身是可聚焦的清單項目（listitem）兼拖曳來源，包一顆按鈕會讓它不能拖也不合語意。

驗收 `node scripts/demo-rename.mjs`：

```
click name      editing=true  value="am-claude"        ← 全選
space in name   invalid=true                            ← 邊框轉紅
Enter (invalid) headerName="am-claude"                  ← 還原，沒送出
Enter (valid)   headerName="前端-1"  sidebar="前端-1,…"  ← 標題與側邊欄一起更新
Escape          headerName="前端-1"                     ← 草稿丟掉
```

```
--- sidebar ---
click other row selected=am-codex  editingInRow=false   ← 選取移動，沒有開編輯器
click its name  editingInRow=true                        ← 已選取的那一列才進編輯
Enter           names="前端-1,後端-2,am-grok"            ← 側邊欄與標題列一起更新
```

截圖 `390-rename-editing.png`、`391-rename-done.png`、`392-rename-sidebar.png`。


## 組隊（TeamLaunchPanel）標題列也掛額度（2026-09-06 加）

組隊是最花額度的一個動作（多個成員各自跑），但 `⚙ 組隊 · <project> · #<issue>` 這條標題列
原本沒有額度——要按「建立並啟動」之前得先切回別的畫面看。現在跟聊天頁 / 群組頁 / team 頁
同一條 `.quota-strip`（`<QuotaStrip host={host} />`，放在 `spacer` 與 `.head-actions` 之間）。

`.team-launch-head` 本來就帶 `team-head`，所以上面那條收縮契約（額度與操作 `flex: none`、
標題讓步）直接適用。量測 `node scripts/demo-launchquota.mjs`：1600 / 1280 / 1100 三個寬度下
額度右緣分別是 1534 / 1214 / 1034，取消鈕 1584 / 1264 / 1084，都在視窗內且無水平捲動。

**同時把面板裡的「額度預覽」卡拿掉**（`QuotaPill` 與 `.team-quota*` 樣式一併刪）：那張卡跟
標題列的額度條是同一組數字，而標題列一直在畫面上。卡片裡原本還放了兩段**擋建立**的警告，
那不是預覽而是「按下去會出事」的理由，所以移到「建立並啟動」正上方（`.team-launch-blocks`），
只有成立時才出現：

- `blockedKind`：某個角色用的 kind 額度已達停手線 → 建立後會馬上暫停。
- `missingCli`：某個角色選的 CLI 在該主機沒安裝（UI 上那個選項本來就是 disabled，這條是防守用）。

驗收把停手線壓到最低（`stop line 1%`）逼出第一條：`blocks:1`、文字完整、
`aboveActions:true`（就貼在按鈕上方，也就是原本那張卡的位置）。
截圖 `370-launch-header-quota.png`、`371-launch-block-alert.png`。


## Bot 標題列：狀態文字換成 pane id（2026-09-06 改）

`執行中 ▾` / `閒置 ▾` 那一格是把左邊的燈號再用文字講一次。狀態看燈就好（`StatusLamp` 自己
帶 tooltip），這格改放 **pane id**——debug 時真正要抄的那一串：

- 有 run 才出現，內容是 `run.pane_id`（例：`w1:pA`）＋一個小 `▾`。
- 點一下展開 / 收合底下的識別列（`.run-debug`：pane、agent、session、workspace、run id，
  每個都可點擊複製），跟原本的開關是同一個。
- tooltip 寫 `pane <id>（<狀態>）· 點一下展開…`，所以狀態文字沒有真的消失，只是不再常駐。
- 顏色保持中性（mono、`--text-dim`），不跟著 lamp 變色——狀態的顏色語彙留給燈號。
- 沒有 run 時整格不渲染（原本會留一段「離線」文字）。燈號已經說了。

驗收 `node scripts/demo-panetoggle.mjs`：

```
stopped bot | slot=null                 lamp title=離線
running bot | slot="w1:pA▾"  detailOpen=false
after click | slot="w1:pA▴"  detailOpen=true
click again | slot="w1:pA▾"  detailOpen=false
```

截圖 `360-pane-toggle-closed.png`、`361-pane-toggle-open.png`。


## 群組標題列：成員只留圖示＋數量、attach 只留群組（2026-09-06 改）

- **`AttachButton` 從 bot 對話頁拿掉**（`ChatPanel` 的兩處：已選 bot 的標題列、與「未選擇 Bot」
  的空狀態列）。herdr session 是**專案層級**的，同一個專案的所有 bot 共用一個，
  所以在每個 bot 的標題列各放一顆只是重複。現在只有群組標題列有（`GroupChatPanel`）。
  另外兩處保留：側邊欄 project 列（滑過才出現）與「環境設定 → 主機」（那是**主機**層級的
  attach 指令，不是專案的）。
- **`MemberStrip` 不再列出成員名字**：一個成員一顆 26px 的圓形 kind 圖示，狀態燈掛在右下角，
  hover 才顯示 `名稱：狀態`，點下去照樣開那個 bot 的單獨對話。名字在側邊欄本來就有。
- **數量搬到圖示旁邊**（`.members-count`）：原本 `N 個成員` 在額度條的另一邊，跟圖示隔著半個
  標題列。專案完整路徑的 tooltip 移到標題的 `<strong title=…>`。

量測（`node scripts/demo-groupmembers.mjs`，先 `cd web && VITE_MOCK=1 npx vite --port 5411 --strictPort`）：

| | 改前 | 改後 |
|---|---|---|
| 3 個成員的 `.members` 寬度 | 約 330px（三顆帶名字的 chip） | **135px**（三顆 26px 圖示＋「3 個成員」） |
| bot 對話頁的 attach 按鈕 | 有 | 無 |
| 群組標題列的 attach 按鈕 | 有 | 有 |
| 點成員圖示 | 開該 bot 的單獨對話 | 一樣（`stillGroup:false`、標題 `am-claude`） |

截圖 `350-group-members-icons.png`。


## 鍵盤換 bot（2026-09-06 新增）

| 按鍵 | 位置 | 行為 |
|---|---|---|
| `↑` / `↓` | 焦點在側邊欄某一列 bot 上 | 換到上／下一個 bot，**焦點跟著跳到新的那一列**，並 `scrollIntoView` |
| `⌥↑` / `⌥↓` | 任何地方（輸入框裡也算） | 換到上／下一個 bot，**焦點留在原地**，打到一半的字不會掉 |
| `⌥↑` / `⌥↓` | 焦點在某一列 bot 上 | 維持原本的「排序」語意（跟相鄰那列交換），不是換 bot |
| `Enter` / `Space` | 焦點在某一列 bot 上 | 選取（原有行為） |

- 順序是**側邊欄看到的順序**：專案依使用者拖出來的 `projectOrder`（`orderedProjects()`），專案內用 `botOrder`
  （`orderedBotIds()`）；頭尾會繞回去（`adjacentBotId()`，兩個都在 `store/store.ts`）。
- 沒有選任何 bot 時（例如正在看群組或 team），`⌥↓` 從第一個開始、`⌥↑` 從最後一個開始。
- 全域監聽器（`App.tsx` 的 `useBotSwitchKeys`）刻意跳過三種情況：`e.defaultPrevented`、
  焦點在 `.bot-row` 內（那裡 `⌥` 是排序）、以及畫面上有 `.modal-backdrop` / `.confirm-backdrop`
  （對話框開著的時候鍵盤是它的）。**單獨的 `↑`/`↓` 全域不攔**——那是輸入框、select、
  終端各自的鍵。
- 選到新的 bot 後焦點會落到 composer（既有行為），所以「⌥↑↓ 挑 bot → 直接打字」是順的。

驗收 `node scripts/demo-botkeys.mjs`（先 `cd web && VITE_MOCK=1 npx vite --port 5411 --strictPort`）：

```
↓ in sidebar  selected=am-codex  focused=am-codex     ← 選取與焦點一起走
plain ↓ (composer)  selected 不變，textarea 內容 "打到一半的字" 保留
⌥↓ (composer)  selected=am-grok   focused=textarea    ← 焦點沒被搶走
↑ from first   selected=am-grok（繞到最後一個）
⌥↓ in a row    順序 am-claude,am-codex → am-codex,am-claude（排序沒被換 bot 蓋掉）
```

截圖 `340-botkeys-sidebar.png`。


## Bot 聊天頁：三排 chrome 併成兩排（2026-09-06 改）

原本標題列下面還疊了兩條各佔一整排、卻都填不滿的橫條：statusline（帳號／模型／context／5h／7d／花費／版本）
與 issues bar（repo chip）。現在合成一排 `.context-bar`：

- **左邊**：issues repo chip；**右邊**：statusline 欄位，中間一條分隔線。
- 只有 statusline 那一半 `overflow-x: auto`。`.issues-pop` 是 `.issues-bar` 內的絕對定位彈窗，
  外面只要有 `overflow` 祖先就會被裁掉，所以捲動不能掛在整排上。
- issues chip 只在「對話」分頁出現（終端沒有輸入框可以插入 issue），statusline 兩個分頁都在。
- 兩邊都沒東西時整排不渲染（`hasStatus` 用傳的，因為 `<StatusLineBar>` 即使 return null 也是個 truthy element）。

順手砍掉重複資訊：

| 原本在 statusline | 現在 |
|---|---|
| `5h 85% · 剩 10m`、`7d 27% · 剩 20h10m` | 拿掉——右上角額度條就是這兩個數字 |
| `模型 Opus 5 · 高 · thinking` | 併進標題列的 model badge（`Opus 5 · 高 · thinking`，`· 高 · thinking` 用 `.model-tag-extra` 淡色） |
| — | badge 原本 `bot.model` 為 null（＝由 CLI 決定）時整個不顯示；現在會退回 statusline 回報的實際模型，用虛線邊框（`.model-tag.reported`）標示「這是 CLI 選的，不是你設定的」 |

標題列的收縮順序也一併補上（跟上一節 Team 標題列同一套契約）：`.main-title` `overflow: hidden`，
bot 名稱 `min-width: 4.5em` 不會被擠不見，model badge `min-width: 4em` 不會縮成空盒子，
⚙ 設定鈕移到名稱後面（cluster 是從尾巴開始裁的，而 badge 都在別處看得到、設定鈕沒有）。

量測（`node scripts/demo-contextbar.mjs`，先 `cd web && VITE_MOCK=1 npx vite --port 5411 --strictPort`；
mock 現在會給 claude bot 一份真實形狀的 statusLine payload）：

| 視窗寬 | header 以下 chrome 高度 | bot 名稱 | ⚙ | quota 右緣 | actions 右緣 |
|---|---|---|---|---|---|
| 1920 | 35px（原本 statusline + issues 兩排） | 可見 74px | 可見 | 1614 | 1904 |
| 1440 | 35px | 可見 63px | 可見 | 1134 | 1424 |
| 1280 | 35px | 可見 63px | 可見 | 974 | 1264 |

issues 彈窗展開高度 468px、沒有被 `.context-bar` 裁到。
截圖：`329-contextbar-after-1920.png`、`330-contextbar-after.png`、
`331-contextbar-issues-open.png`、`332-contextbar-terminal.png`。


## 標題列（`.main-head`）的收縮契約（2026-09-06 修）

Team 面板長標題（`Team · #1 群組時間軸的 ULID 同毫秒排序隱患…`）加上「已暫停・成員啟動失敗」與
「交付：留分支」兩顆 badge 時，右邊的額度（`.quota-strip`）與操作按鈕會被推出視窗、cc1 只剩半截。

根因：`.group-head .main-title { flex: none }` 也套到 TeamPanel（`main-head group-head team-head`），
所以整串標題＋badge 完全不能收縮，只能把右邊的東西往外推。

契約（`web/src/styles.css`）：

- `.main-head > .quota-strip`、`.main-head > .head-actions`：`flex: none`，**永遠完整可見**。
- `.team-head .main-title`：`flex: 1 1 auto; min-width: 0; overflow: hidden`。
  `overflow: hidden` 是必要的——badge 有 padding 撐出的寬度下限，少了它一旦被擠扁就會畫到
  BudgetMeter 上面。
- `.team-head .main-title strong`：`min-width: 6em`，標題可以被截斷但不會整個消失
  （「現在在哪個 team」是最不能不見的資訊），完整標題留在 tooltip。
- `.team-head .team-phase / .team-deliver`：`flex: 0 1 auto` + ellipsis，空間不夠時先讓步。
- GroupChatPanel 的 `.group-head .main-title { flex: none }` 沒動，ChatPanel 也沒動。

量測（`node scripts/verify-main-head-layout.mjs`，先 `cd web && VITE_MOCK=1 npx vite --port 5411 --strictPort`，
腳本會自己在 mock 裡建一個 team、注入問題標題，並用同一份 CSS 的 `!important` 還原修正前狀態當對照）：

| 視窗寬 | 修正前 quota 右緣 | 修正後 quota 右緣 | 修正後 actions 右緣 |
|---|---|---|---|
| 1440 | 1679（超出 +427px） | 1236 | 1424 |
| 1100 | 1564（超出 +652px） | 900 | 1088 |

1800 / 1600 / 1440 / 1280 / 1100 / 1024 全部 `overflow none`、無水平捲動。
截圖：`main-head-quota-clip-before(-1100).png`、`main-head-quota-clip-after(-1100).png`、
`main-head-group-chat-after.png`、`main-head-chat-after.png`。

已知限制：約 1150px 以下，`budget + quota + actions` 三塊固定寬度就吃掉整條標題列，標題會被完全裁掉。
`QuotaStrip` 的 `collapsed`（`width < 1100`）目前只換樣式、**寬度仍是 434px**，沒有真的省空間；
要讓窄視窗也看得到標題，得先讓 collapsed 真的縮寬（另案）。


## 新增 Project / 新增 Bot / 環境設定 = popup（2026-09-06 改）

這三個原本都長在側欄裡：前兩個把整個側欄換掉（頂部一顆「←」返回），環境設定是底部的
disclosure 往上展開、把 bot 清單擠掉。現在統一改成置中 popup（`components/Modal.tsx`）：

- 背景是半透明遮罩，**bot 清單留在後面看得到**，關掉就回到原本的捲動位置。
- 三種關法一致：`Esc`、右上角 `✕`、點遮罩。popup 內層若自己吃掉 `Esc`（例如目錄選擇器會先
  退回表單），`Modal` 看 `defaultPrevented` 就不會跟著關 —— 一次只關一層。
- 開啟時自動 focus body 裡第一個可輸入元素。
- body 沿用 `.sheet-body` class，所以 P1 那批表單樣式（欄位高 40px、間距 16px、
  `.sheet-body:has(> .dirpicker)` 的撐高規則）原封不動繼續套用。
- 「環境設定」popup 寬 560，內容分成「主機 / 身分 / 顯示」三段（`.env-sec`），
  比原本擠在側欄底部好讀；側欄底部只剩一顆帶齒輪 icon 的按鈕與摘要文字。

`.new-bot-sheet` / `.new-project-sheet` / `.sheet-head` 這三個 class 隨之退場。
驗收腳本 `scripts/demo-popups.mjs`：

| 檔案 | 內容 |
|---|---|
| `320-popup-project.png` | 新增 Project popup，後面看得到 bot 清單 |
| `321-popup-project-picker.png` | popup 內開目錄選擇器：660×760、清單高 523px、底部按鈕仍在畫面內 |
| `322-popup-bot.png` | 新增 Bot popup，標題列帶 project 名稱；高度隨內容收合 |
| `323-popup-env.png` | 環境設定 popup，主機／身分／顯示三段 |
| `324-popup-env-dark.png` | 同上，深色主題 |

腳本同時斷言：選擇器內按 `Esc` 只退回表單（popup 仍開），再按一次才關；`✕` 與點遮罩都會關。


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

- **「環境設定」popup 裡的「主機」段**（`components/HostsPanel.tsx`）：側欄底部按鈕右側顯示 `本機 + N ・ M 個未連線`。
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
| **模型** | `<select>`：`（預設）`＋常用別名＋`自訂…`。claude = `opus` / `sonnet` / `haiku`，codex = `gpt-5.6-sol` / `gpt-5.6-luna` / `gpt-6-astra`（`gpt-5.5` 2026-09-09 起不進選單，見 `HIDDEN_MODELS`；已經設成它的 bot 那顆按鈕照樣留著），grok = `grok-4.6` / `grok-4.5`。選「自訂…」多出一個文字框，可送任意字串；清空 = `null` |
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


## 專案群組聊天（SPEC §13 / API.md §11，2026-09-06 新增）

一個 Project 就是一個群組。契約以 `docs/API.md` §11 為準。

### 資料流

| 來源 | 前端處理 |
|---|---|
| `GET /projects/:id/messages` | `api.fetchProjectMessages()` → `normalize.toGroupMessagesPage()`（每則帶 `bot_id` / `bot_name`），存 `store.groupMessages[projectId]`，以 `message.id` 排序 |
| `POST /projects/:id/chat` | `api.sendGroupChat()` → `store.sendGroupChat()`；回來的 `sent[]` 先塞進各 bot 的 `turns` map（輸入框立即反映 in-flight），`skipped[]` 跳一則通知；`400 no_mention` 以 `ApiError` 顯示 |
| WS `message_added` | 既有的 per-bot 路徑不變，**同一 frame** 再依 `bot.project_id` 追加到 `groupMessages`（已載入時），並在該群組視圖未開啟時把 `groupUnread[projectId]` +1（只算非 user 訊息） |
| `resync` | 重載目前 bot 的 messages 之外，也重載目前開著的群組 |

`Message` 型別新增 `group_id: string | null`；`normalize.toMessage()` 讀 `group_id` / `groupId`。
mention 規則放在 **`web/src/api/mentions.ts`**（`parseMentions()`），與 `daemon/src/group.rs` 同一套，
mock 與輸入框共用；後端仍是最終裁決者。

### store

- `selectedProjectId`：非 null = 右側顯示群組視圖（`App.tsx` 據此切 `GroupChatPanel` / `ChatPanel`）。
  `selectBot()` / `openSettings()` 會清掉它；`selectProject(id)` 順便把 `groupUnread[id]` 歸零並載入時間軸。
- `groupComposerState(state, projectId)`：**專案內至少一個 bot 的 `composerState()` 未鎖定就可送**；
  `sendable` 列出目前可收的 bot id，輸入框用它標示「會被略過」的收件者。

### UI（`components/GroupChatPanel.tsx`）

- **sidebar**：Project 標題變成按鈕（`⌗` 圖示 + 名稱），點了切群組視圖，選取時變 accent 色；
  右邊的藍色圓點是未讀計數（`.unread-badge`）。
- **標題列**：`⌗ <label>` + `群組` 標籤 + host 徽章 + **成員燈號列**（每個成員一顆燈 + 名稱徽章，
  點了跳到該 bot 的單獨對話）+ 成員數 / 路徑 + 「關閉群組」。≤1080 寬時成員列收起。
- **時間軸**：`foldRows()` 把同一 `group_id` 的 user 副本折成一列，`from` 顯示 `→ @a, @b`；
  單一 bot 對話送出的訊息（無 `group_id`）也顯示 `→ @bot`。assistant / system 訊息上方是
  bot 名稱徽章（`.bot-badge.claude` / `.codex`，配色沿用 kind-tag）。氣泡本體重用
  `ChatPanel.tsx` 的 `Bubble`（新增可選的 `from` prop）。每個仍在回覆中的成員各一個 typing 氣泡。
- **輸入框**：`mentionAtCaret()` 找游標前的 `@token`，彈出 `@all` + 成員（依前綴過濾）；
  ↑ / ↓ 移動、Enter / Tab 選取（插入 `@name `）、Esc 關閉；其餘 Enter 送出、Shift+Enter 換行。
  沒有 mention → 送出鈕 disabled，下方黃字提示；有 mention → 列出 `→ @a, @b`，並標示目前無法接收、
  會被略過的成員（不會自動啟動）。

### mock（`VITE_MOCK=1`）

`api/mock.ts` 實作兩個群組端點（mention 解析、fan-out、`skipped` + system 註記、`group_id`），
`prompt` 的 409 `reason` 改成與 daemon 相同的英文字串，方便 `skipped.reason` 分類一致。

### 驗收紀錄（2026-09-06，mock）

`node scripts/demo-group.mjs`（先在 `web/` 內 `VITE_MOCK=1 npx vite --port 5185`；CDP 埠 9377、
獨立 user-data-dir，避免撞到其他 agent 的 headless Chrome）：

| 檔案 | 內容 |
|---|---|
| `140-group-empty.png` | 兩個 bot 啟動後點 Project 標題進群組視圖：標題列成員燈號、空時間軸（只有 run 啟動的 system 訊息） |
| `141-group-no-mention.png` | 輸入沒有 `@` 的文字：送出鈕 disabled + 提示 |
| `142-group-mention-popup.png` | 輸入 `@`：自動完成列出 `@all` / `@am-claude` / `@am-codex`；↓↓ Enter 選到 `@am-codex ` |
| `143-group-all-working.png` / `144-group-all-replied.png` | `@all Reply with exactly GROUP-OK`：user 氣泡折成一則 `→ @am-claude, @am-codex`，兩個 typing 指示 → 兩則帶 bot 徽章的 Markdown 回覆 |
| `145-group-single-target.png` | `@am-claude, only you: reply PONG`：只有 am-claude 收到（`→ @am-claude`） |
| `146-group-skip-hint.png` / `147-group-skipped.png` | 停掉 am-codex 後 `@all`：輸入框提示「@am-codex 目前無法接收，會被略過」；送出後時間軸出現 am-codex 的「群組訊息未送達」system 註記 + 右下通知，am-claude 照常回覆 |
| `148-group-unread-badge.png` / `149-group-after-direct.png` | 切到 am-claude 單獨對話送一則，回覆到達時 Project 標題出現未讀 `1`；再開群組視圖歸零，該則以 `→ @am-claude` 出現在時間軸 |
| `14a-group-dark.png` / `14b-group-narrow-900.png` | 深色主題、900 寬 |

真後端的端到端（兩個真 bot `@all` → 兩個 `GROUP-OK`、`@g-claude` 單送、停掉 g-codex 後 `skipped`、
400 `no_mention`、分頁）以 curl 對 `AM_DATA_DIR=/tmp/am-group` 的獨立 daemon（7799）驗過，見 SPEC §13.7。

### 已知問題（群組聊天）

1. **群組時間軸只包含存活的 bot**：刪掉 bot 後它的歷史仍在 `GET /bots/:id/messages`，但不再出現在群組合併時間軸。
2. **沒有「載入更早訊息」**：`GET /projects/:id/messages` 的 `before` / `has_more` 契約與 `api.fetchProjectMessages(…, before)` 都備妥，UI 一次抓 200 則。
3. **未讀計數只在記憶體**：重新整理即歸零；`GET /api/state` 的 `unread` 仍固定 0。
4. **mention 自動完成只針對游標前的 token**：在文字中間插入 `@` 後往回移動游標也會觸發，但用滑鼠選取整段文字時不會重新計算游標。
5. **mock 的 `@all` 回覆順序固定**（依 `REPLIES` 輪替），真後端的順序取決於各 agent 的回覆速度。

## grok kind（2026-09-06，SPEC §12）

`BotKind` 加上 `'grok'`（`BOT_KINDS` 常數供 normalize / mock 共用）。新增 Bot 與新增身份的 kind 下拉多了 `grok`；`MODEL_OPTIONS.grok = ['grok-4.6', 'grok-4.5']`；模型欄位標題對 claude 顯示 `--model <值>`、其餘（codex / grok）顯示 `-m <值>`；auto_approve 說明改用 `AutoApproveFlags`（claude `--dangerously-skip-permissions` / codex `--yolo` / grok `--always-approve`）；`.kind-tag.grok` 灰色系（深淺色各一組）。mock 多種一個 `am-grok`。身份表單 env 提示補上 `GROK_HOME`。

驗收腳本 `scripts/demo-grok.mjs`（mock 模式，`VITE_MOCK=1 npx vite --port 5186`）：

| 檔案 | 內容 |
|---|---|
| `130-grok-kind-tag.png` | sidebar 三個 bot：`am-claude` / `am-codex` / `am-grok`，各自的 kind 標籤 |
| `130-new-bot-grok-form.png` | 「+」新增 Bot 表單 kind 選 `grok`：模型選項 `（預設） | grok-4.6 | grok-4.5 | 自訂…`，auto_approve 文案含 `grok --always-approve` |
| `131-grok-bot-added.png` | 選 `grok-4.5` 送出後列表出現 `am-grok-2` |


## UI 優化（2026-09-06，視覺與互動）與即時輸出（API.md v3.9）

以真 daemon 截圖 `170–174` 為輸入的整理；只動 `web/src/**`，驗收腳本依賴的 class / 結構
（`.app` `.bot-row` `.bot-name` `.lamp.lamp-<state>` `.kind-tag.<kind>` `.identity-badge` `.host-badge`
`.project-head` `.project-label-btn` `.icon-btn.add` `.inline-form` `.opt-group .opt` `.msg(.assistant/.user)`
`.bubble(.md)` `.msg-list` `.composer textarea` `.composer-lock` `.dirpicker*` `.host-row` `.identity-row`
`.disclosure` `.tab`）全部保留。

### 改了什麼

| # | 問題 | 處理 |
|---|---|---|
| 1 | 訊息垂直留白過大 | `.msg-list` gap 12→6px、氣泡 padding 8/12→6/11、行高 1.45；meta 列縮成 10px 單行（`<time>` + 來源標籤）；同側相鄰訊息（`.msg.user + .msg.user`、`.msg.assistant + .msg.assistant`）再收 4px。一對短問答從 ~200px 降到 ~110px |
| 2 | `1` 被畫成框中框 | fenced code 改為無邊框淡色底（`--code-bg`）；整則回覆只有一個 fence 時（`.bubble.md > pre:only-child`）直接以等寬字顯示，不畫底。行內 code 也去邊框 |
| 3 | 群組 bot 徽章 / 收件者列佔一整行 | `from` 改進 meta 列：回覆為 `[bot 徽章] 時間 · hook`，使用者訊息為 `時間 → @a, @b`（`.msg-from` 仍在，只是位置改到 `.msg-meta` 內；system 註記無 meta 列，徽章維持在氣泡上方）。補上 `.bot-badge.grok` 配色 |
| 4 | 淺色偏白、對比弱；深色側欄邊界不明 | 新增 `--bg-side`：淺色側欄 `#f4f5f8`、對話底 `#e9ecf1`、白色氣泡 + 邊框 + 1px 陰影；深色側欄 `#131519`（最深）、對話底 `#191c22`、標題列 / 輸入區 `#1f232a`；側欄右邊線改 `--border-strong` |
| 5 | 輸入框下說明文字 | 單 bot 的 `client_request_id` 說明拿掉；群組的固定說明拿掉，只在「沒有 mention」或「已解析收件者 / 略過」時顯示；placeholder 縮成「輸入訊息…」「@bot 或 @all …」，Enter / Shift+Enter 說明移到 `title` |
| 6 | 側欄每列擁擠 | ⚙ 只在 hover / focus / 選取時出現（`@media (hover: none)` 常顯）；狀態字 `.bot-state.<lamp>` 在 idle / offline 時隱藏（燈號已足夠），working 藍、blocked 紅、starting / stopping 黃；`.bot-sub` 不換行、溢出截斷；停止 / 啟動鈕改透明底、固定最小寬 |
| 7 | 標題列資訊過多 | `run <id> ・ pane <id>` 收進狀態字的 `title`；狀態字依 lamp 上色（`.main-status.working/.blocked`）；標題列固定 46px 與側欄頭齊高 |
| 8 | 空狀態 / 載入 / toast 不一致 | 共用 `EmptyState`（`.msg-empty`，虛線卡片；`loading` 版帶 typing 點）用於無選取、無訊息、終端未執行、群組空；toast `.notice` 統一左側色條（info 藍 / error 紅）、進場動畫、關閉鈕 hover |
| + | 長訊息預先收合 | 依使用者要求移除 `isLong` / `.clamped` / 「展開全文」，一律完整顯示 |
| + | Bot 設定面板 | 頭列 40px、欄位改白底 + 強邊框，其餘不變 |

### 即時輸出（WS `turn_progress`）

- **store**：`liveReply: Record<botId, {turnId, text, activity, revision}>`。`turn_progress` 更新（同 turn 只接受 revision 不倒退）；
  同 turn 的 assistant `message_added`、或 `turn_updated` 離開 `in_flight` 時清掉（清除時機未變）。
  選擇器 `liveReplyOf(state, botId)` 只在該 turn 仍是 `inFlightTurn`，且 **`text` 或 `activity` 至少一個非空白**時回傳。
  > v4.1 前的守衛是「`text` 非空白」，會把「只有 activity、還沒有 text」的幀整個丟掉——正是純思考階段氣泡卡在
  > 「等待回覆（hook）…」的直接原因。
- **ChatPanel / GroupChatPanel**：訊息列最後的 `LiveBubble`（`.msg.assistant.live`），meta 三態：
  1. 有 `text` → `.streaming`，以 Markdown 渲染、邊框帶 accent、右下角閃爍游標、meta 顯示「輸出中…」；
  2. 沒有 `text` 但有 `activity` → 氣泡本體維持 typing 點，meta 直接顯示該活動字串（例如
     `Thinking… (12s · ↑ 1.2k tokens)`）。**activity 來自終端畫面，當純文字渲染，不走 Markdown。**
  3. 兩者皆無 → typing 點 +「等待回覆（hook）…」。

  群組視圖每個進行中的成員各一個（`liveText[botId]` / `liveActivity[botId]`），帶 bot 徽章。內容變長時只在
  使用者原本就在底部（距底 < 80px）才自動捲到底。
- **mock**：`prompt` 後先每 0.2 秒推 2 幀「只有 `activity`、`text` 為空」的思考幀（`Thinking… (2s …)`、
  `Reading files… (4s …)`），再每 0.5 秒推 3 幀（`slow` 4 幀）`turn_progress`（回覆前綴 + `activity: 'Writing…'`），
  約 2.4 秒送最終 `message_added`。`REPLIES` 多一則 ``` ```\n1\n``` ``` 用來驗證單 fence 的顯示。
  → `VITE_MOCK=1` 可直接重現並驗收三態。

### 驗收截圖（UI polish；亦見下方 v4.0 重拍）

mock（`VITE_MOCK=1 npx vite --port 5186`，headless Chrome CDP 9360，1440×900 @2x）：

| 檔案 | 內容 |
|---|---|
| `180-chat-dark.png` / `180-chat-light.png` | 單 bot 對話（含 v4.0 額度列 / attach / Issues） |
| `180-live-chat-dark.png` | 送出途中：即時氣泡 |
| `180-live-group-dark.png` | 群組 `@all`：成員即時氣泡 |
| `180-group-dark.png` / `180-group-light.png` | 群組時間軸 |
| `180-settings-dark.png` / `180-settings-light.png` | Bot 設定（含 persona / Fast） |
| `180-newbot-dark.png` / `180-newbot-light.png` | Project「+」新增 Bot（無 Project 選擇） |
| `180-issues-dark.png` / `180-hosts-tools-dark.png` | Issues 面板、主機工具徽章（v4.0） |
| `180-chat-narrow-900.png` | 900 寬 |

真後端（`npx vite --port 5187` → 7788，CDP 9361；舊 daemon 無 v4.0 端點時 UI 優雅退回）：

| 檔案 | 內容 |
|---|---|
| `181-chat-dark.png` / `181-chat-light.png` | 真資料對話（無 quota／Issues 時不顯示） |
| `181-live-chat-dark.png` / `181-chat-after-live-dark.png` | 即時輸出與回合結束 |
| `181-group-dark.png` / `181-group-light.png` | 群組 |
| `181-settings-dark.png` / `181-newbot-dark.png` | 設定 / 新增 Bot |

腳本：`node scripts/demo-v40.mjs`（`AM_URL` / `AM_PREFIX` / `AM_CDP` 可覆寫）。

### 已知取捨

1. **即時氣泡的前幾幀可能是 TUI 雜訊**（真後端看到 `✢ Improvising…`、`Tip: …` 各出現約 0.7 秒），
   這是 daemon 端 `turn_progress` 的過濾範圍；前端只把全空白的幀當成「沒有文字」。

   反向的情況更痛：**過濾太乾淨時整幀變空**。agent 在純思考／跑工具階段，畫面上只有 spinner 行、框線與
   狀態列，全被 `clean_screen()` / `is_noise()` 濾掉 → `text` 一直是空字串、與上一幀相同 → daemon 一幀都不發 →
   氣泡永遠停在「等待回覆（hook）…」，思考愈久空窗愈長。v4.1 的解法是**旁路**而非放寬過濾（`clean_screen()`
   同時餵最終回覆的 `terminal_fallback`，動不得）：daemon 另外算 `live_activity()` 取該回合最後一行 spinner 行，
   以獨立的 `turn_progress.activity` 欄位送出，`text` 或 `activity` 任一變化就發幀；前端把它顯示在 meta 列，
   不進氣泡本文、不進 DB。

   真實現場樣本：卡住 3 分鐘時 pane 上只有 `✻ Boogieing… (3m 18s · ↓ 11.0k tokens)`。**動詞是隨機挑的**
   （`Thinking` / `Boogieing` / `Improvising` / `Puttering` / `Simmering`…），**不可字面比對**；glyph 集合
   （`✻ ✽ ✶ ✳ ✢ ·`，grok `◆`）也會隨版本增減，所以另有一條 glyph-independent 的形狀比對
   （`<單字>… (…)` + 括號內含 `tokens` 或 `12s` / `3m` 時間樣式），兩條取聯集。
   括號內秒數每 0.7 秒都在變 → `activity` 幾乎每幀都不同、每幀都發，這是刻意的（計時會跳），**不加去抖／節流**。
2. **不同 bot 的相鄰回覆**在群組視圖也套用同側收緊（-4px），靠 meta 列的徽章區分。
3. **狀態字隱藏 idle / offline** 後，離線 bot 只靠空心燈號辨識；hover 列或看啟動 / 停止鈕可確認。
4. **≤1080 寬時標題列的狀態字整個隱藏**（沿用原規則），run / pane tooltip 也跟著不可見。

---

## 8. v4.0（API.md §12，2026-09-06）

契約見 [`docs/API.md`](./API.md) §12。前端只動 `web/src/**`；舊 daemon（例如目前 7788
尚未實作 `/api/models`、`/api/quota`、`attach_command`、`tools`、`github`、`persona`、`fast`）
必須能開、能聊，v4.0 區塊靜默消失或退回靜態清單。

### 8.1 Attach 指令

- `hosts[].attach_command`（本機在 `GET /state` 的 reserved `local` host；normalize 合成到
  `AppState.attach_command`）。缺欄時前端依 ssh / session 合成。
- `AttachButton`：標題列一鍵複製到剪貼簿，並短暫展開彈層顯示指令。掛在
  `ChatPanel` / `GroupChatPanel` 標題列、側欄專案列、`HostsPanel`。

### 8.2 側欄專案列

- `ProjectTitle`（`.project-label-btn`）整列可點開群組聊天，`min-height: 32px`、hover 底色。
- 專案「＋」開 `NewBotForm({ initialProjectId })`：**不顯示 Project 選擇**，直接 `autoFocus` 名稱。

### 8.3 模型 / effort / Fast

- `GET /api/models?kind=&host=&identity=` → `ModelPicker.ApiModelFields`（三種 kind 都走它）。
  選模型後顯示該模型的 `efforts`；`service_tiers` 含 `priority` 才顯示 Fast 開關。
  切到無 Fast 的模型會清掉 `fast`。
- claude 的模型是 daemon 端的靜態清單（opus / sonnet / haiku / fable），**v4.1 起帶
  `--effort` 的五級**（low / medium / high / xhigh / max，每個 alias 都一樣）。因為不隨模型
  變，強度那列不顯示「依 <模型>」的註記；執行中改強度會走 TUI 的 `/effort <level>` 當場套用
  （`liveEffort()` 現在含 claude），註記寫「執行中改會即時套用，不用重啟」，tooltip 補一句
  claude 會順手把它存成之後新 session 的預設。
- 失敗或舊 daemon（可能回 SPA HTML 200）：退回 `MODEL_OPTIONS`；codex 另帶
  `CODEX_EFFORT_OPTIONS`，grok 帶 `EFFORT_OPTIONS`，claude 帶 `CLAUDE_EFFORT_OPTIONS`。
  Fast 在靜態清單不顯示（無 tiers）。
- 截圖：`353-claude-effort.png`（`node scripts/demo-claude-effort.mjs`）。
- `bot.fast` / `bot.persona` 貫穿 types → normalize → mock → 新增表單 → 設定面板。

**「預設」提示現在會提示（v4.2，SPEC §17.1）** — claude 的 `default_effort` 不是模型內建的，
是那個身份的 `settings.json`（`effortLevel` 全域 + per-model 覆寫），所以「預設」按鈕改身份
要跟著換數字：

- `ApiModelFields` / `ModelQuickPicker` 都吃一個 `identity?: string | null`，跟著它一起放進
  `store.models` 的快取 key（`${kind}@${host}@${identity}`）——不放的話切身份不會重抓，會沿用
  上一個身份的提示。四個呼叫點（`BotSettingsPanel`、`Sidebar` 新增 Bot 表單、
  `TeamLaunchPanel`、`TeamPanel.WorkersChip`）都已經把當下選的 `identity` 傳進去；
  `ModelQuickPicker` 直接讀 `bot.identity`（它本來就對著一個現成的 bot）。
- tooltip 用 `defaultEffortNote(kind)` 分開講：codex / grok 寫「模型預設」，claude 寫
  「帳號目前設定」——同一個模型換帳號會不一樣，不能講成模型內建的。旗標名稱也分開
  （`effortFlagName`：claude 是 `--effort`，其餘是 `--reasoning-effort`）。
- **標記不只藏在 tooltip 裡**（2026-09-07 加）：`default_effort` 對到的那顆強度按鈕上直接掛一個
  「廠推薦」小標（`.effort-recommended`），三種 kind 共用同一段渲染邏輯，所以 codex / grok 跟
  claude 一樣不必 hover 就看得到——不管 API/帳號設定回報的是哪一級，永遠精準標在那一顆上，不是
  寫死某個特定等級。`ApiModelFields`（完整表單）與 `ModelQuickPicker`（標題列的緊湊版）都有。
  截圖 `366-effort-mark-claude/codex/grok.png`（`node scripts/demo-effort-mark.mjs`）。
- 驗收 `node scripts/demo-effort-default-hint.mjs`：不指定身份「預設（高）」→ 選 Opus 後帳號的
  per-model 覆寫蓋過全域，變「預設（低）」→ 切到 cc1（只有全域）變「預設（中）」→ 切到 cc2
  （什麼都沒設過）落回 claude 內建預設，仍是「預設（高）」，**不是**空白的「預設」——這點初版做
  錯過（回 `null`），查了 claude 官方文件（`code.claude.com/docs/en/model-config`）加真機驗證
  才發現 `high` 才是沒設定時真正會發生的事。截圖 `362`〜`365`。

### 8.4 額度列

- `GET /api/quota` + WS `quota_updated` → store `quota`；`QuotaStrip` 置中於 Chat / Group
  標題列（**不在** `App.tsx`）。
- 顯示 `5h` / `7d` 血條（有回報的視窗才畫）；剩餘低於門檻警示色；hover 顯示重置時間與 plan。
- `claude:<identity>`（例如 `claude:cc1`）各自獨立一條 gauge，kind 圖示旁以小字標身份名稱；
  與預設帳號的 `claude` 列並存（cc0 / cc1 都在時就會看到兩條 Claude）。
- 失敗 / 空 map → 整列不渲染。

**一次一台主機**（SPEC §14）。store 的 `quota` 是全部主機的合併 map，key 在遠端帶 `<host>/` 前綴
（`m4p/claude:cc1`），每筆另有 `host`。`QuotaStrip` 收一個 `host` prop：

- 來源：ChatPanel → 該 bot 專案的 host；GroupChatPanel → Project 的 host；TeamPanel → Team 專案的
  host；沒選任何東西 → `local`。
- 元件內先 `scopeToHost()` 把 map 投影成該主機的裸 key，原本那套「cc0 → cc1 → codex → grok」的固定
  排序完全不必知道主機存在；讀 store 時再用 `quotaKey(host, base)` 組回完整 key。
- 遠端在條的最左邊掛一個主機名牌 `.quota-host`，**本機不掛**（預設狀態，多一個「本機」只會吃掉標題列
  寬度；那裡本來就有 `HostBadge`）。gauge 的 tooltip 一律以主機名開頭，popover 標題是「本機額度」/
  「m4p 的額度」。
- 側欄 bot 列的 critical 警告（`botQuotaWarning`）也吃 host，遠端 bot 讀它自己那台的列。
- 截圖：`340-quota-host-local.png`（本機）、`341-quota-host-remote.png`（m4p）、
  `342-quota-host-remote-pop.png`（popover）、`343`〜`345`（切回本機 / 深色 / 1040 寬）；
  重跑 `node scripts/demo-quota-host.mjs`（需 `VITE_MOCK=1 npx vite --port 5311`）。

### 8.4.1 身份清單的來源（SPEC §16）

- store 的 `identities` 仍是 config.toml 那一份；每台主機另外有 `hosts[].identity_status`
  （本機是 `localIdentityStatus`），裡面除了登入狀態還帶 `source`（`config` / `shell`）與
  `config_dir`。
- `identitiesOfHost(all, status)`（`store.ts`）把兩者合起來：config 先，同名的 shell 身份讓位——
  和 daemon 的 `tools::identities_for_host` 同一條規則。它是**純函式**，元件端用 `useMemo`
  （回新陣列的 selector 會無限 re-render，React #185）。
- 用它的地方：`IdentityOptions`（新增 Bot / Bot 設定 / 開團的身份選項，吃 bot 會跑的那台 host）、
  `QuotaStrip`（額度列一次一台，身份也要跟著那一台）、側欄腳的身份計數（本機）。
- `IdentitiesPanel` 下半段列出各主機 shell 認來的身份（`.identity-shell-block`）：唯讀，沒有刪除鈕，
  寫明是哪台主機、指到哪個 `CLAUDE_CONFIG_DIR`。同名被 config 蓋掉的不重複列。
- 截圖：`350-identities-shell-local.png`、`351-identities-shell-two-hosts.png`、
  `352-bot-settings-identity-options.png`；重跑 `node scripts/demo-identity-shell.mjs`。

### 8.5 工具偵測與安裝

- `hosts[].tools`（缺欄 → `TOOL_UNKNOWN` / `installed: true`，避免舊 daemon 誤報缺工具）。
- `ToolBadges` 在 `HostsPanel`；`ToolsHint` 可收合提示列掛在標題列下方，選 running bot →
  `POST /hosts/:name/tools/install`。
- 新增 Bot 表單：該 host 未安裝的 kind `disabled`，旁附小「安裝」鈕。

### 8.6 kind 圖示 / 文字

- store `kindDisplay`（`localStorage` key `am.kindDisplay`），側欄腳 `KindDisplayToggle`。
- 所有 kind 標示走 `KindTag`（class 仍為 `.kind-tag.<kind>`，驗收腳本可辨識）。

### 8.7 對話草稿

- store `drafts` + `localStorage`（`am.drafts`）；key `bot:<id>` / `group:<projectId>`。
- 切換 bot／群組／重整保留；**送出成功**才清空；`delivery=failed` 保留。
- 刪 bot 清對應 draft；刪專案清 `group:` 與該專案下各 `bot:` draft。

### 8.8 人設 persona

- 設定面板與新增表單的 `PersonaField`（textarea）；有值時側欄／標題列顯示 `PersonaMark`。

### 8.9 GitHub Issues

- `projects[].github` 非 null 時，對話頂部 `IssuesBar`：搜尋（300ms debounce）、open/closed、
  labels 顯示、插入 `#n 標題\nurl`、插入完整內容為 `>` 引用。
- 舊 daemon 無 `github` → 不渲染；`gh` 502 顯示錯誤字。

### 8.10 舊 daemon 退回一覽

| 端點 / 欄位 | 行為 |
|---|---|
| `GET /quota` 失敗或回 HTML | `quota={}`，額度列不顯示 |
| `GET /models` 失敗或空清單 | 靜態模型 + kind 對應 effort |
| `attach_command` 缺 | 依 host 合成 |
| `tools` 缺 | 視為已安裝（不誤報） |
| `github` 缺 | 無 IssuesBar |
| `persona` / `fast` 缺 | 讀成 null / false；寫入多半被舊 daemon 忽略 |

## Project 選擇 UI（grok）

優化側欄 Project 標題列與新增 Bot 表單的 Project 選擇（只動 `web/src/**`）。

### 改了什麼

| # | 項目 | 處理 |
|---|---|---|
| 1 | 新增 Bot 的 Project 欄 | `NewBotForm` 拿掉原生 `<select>`，改成與 kind 相同的 `.opt-group` / `.opt` chip 列；每個 Project 一顆 chip 顯示 `label`，遠端 Project 在 chip 內帶既有 `HostBadge`（`@host`）；主機未連線的 chip `disabled`。有 `initialProjectId` 時仍預選該 Project |
| 2 | 超過 6 個 Project | chip 列上方出現小過濾框（`.opt-filter`），輸入即時依 `label` 過濾 |
| 3 | 側欄 `.project-head` | 整列可點（點路徑區也會 `selectProject`；`＋` / `✕` / 標題鈕仍 `stopPropagation`）；hover 有 `--bg-hover`；`min-height: 32px`；`store.selectedProjectId` 對應的列加 `.selected`，左側 3px `--accent` 色條。class 名稱（`.project-head`、`.project-label-btn`、`.icon-btn.add`、`.host-badge`、`.opt-group`、`.opt`）全部保留，只調結構與 CSS |
| 4 | 深淺色 | chip / 色條 / hover 一律走既有 CSS 變數（`--accent`、`--bg-hover`、`--bg-active` 等）；選中 chip 內的 `HostBadge` 改用 `accent-text` 混色以保持對比 |
| 5 | 對比 / 對齊 / 換行 / hover | chip `max-width: 100%` + label ellipsis，窄側欄可換行不橫向溢出；disabled chip 不用半透明疊加（改明確淡色）；`.project-head:hover:not(.selected)` 與 `.selected:hover` 分開；列上 `＋`/`✕` hover 底色加色混以免被列 hover 洗掉 |

### 驗收截圖

mock（`VITE_MOCK=1 npx vite --port 5190`，headless Chrome CDP 9370，獨立 `--user-data-dir`，@2x）：

| 檔案 | 內容 |
|---|---|
| `190-project-select-dark-1280.png` / `190-project-select-light-1280.png` | 多 Project + 遠端 badge；footer「新增 Bot」chip 列（含 filter、disabled `@dead`）；側欄 `.project-head` selected / hover |
| `190-project-select-dark-900.png` / `190-project-select-light-900.png` | 900 寬（側欄 244px）chip 換行 |
| `190-project-select-inline-dark-1280.png` / `190-project-select-inline-light-1280.png` | Project 列「＋」展開的 inline 表單（`initialProjectId` 預選） |

### 檔案

- `web/src/components/Sidebar.tsx` — `NewBotForm` Project chip + 過濾；`.project-head` 整列點選與 `.selected`
- `web/src/styles.css` — `.project-head` / `.selected`、`.opt-filter`、chip 換行 / disabled / hover、chip 內 `HostBadge` 對比

## UI 決策實作（Codex sol 決策 → grok 實作，2026-09-06）

決策清單見 `docs/UI-DECISIONS.md`（12 條，依實機截圖 200–204 產出）。實作分四個 commit：P0、P1、P2，加一個 zustand selector 修正。

| 決策 | 對應截圖 | 狀態 |
|---|---|---|
| P0 群組收件者／發言者 | `212-ui-group-dark` | 訊息上方 metadata 列（`你 → am-claude` / `am-claude · Claude`）、composer 上方收件者 chip（`@all · 3 個 bot`） |
| P0 停止／刪除誤操作 | `210-ui-chat-dark`、`214-ui-settings-dark` | bot 列不再常駐紅色停止鈕（改 hover 選單）、`ConfirmDialog.tsx` 帶全名確認 |
| P1 版面寬度與動線 | 全部 | 量測：lane 1152px、agent 氣泡上限 840px |
| P1 側欄資訊層級 | `210`、`212` | 專案兩行（名稱＋路徑）、bot 列 kind 圖示化、底部只留兩顆主要按鈕＋「環境設定」 |
| P1 缺少 CLI 提示 | `210` 頂欄 | 由 140px 區塊縮成琥珀色 `⚠ 1` 圖示 |
| P1 額度 pill | `210`、`212` | 單一 kind pill＋「全部額度」popover（<1600px 時收合） |
| P1 淺色對比 | `211-ui-chat-light` | canvas／surface／border 角色重建 |
| P1 訊息結構 | 全部 | 量測：同回合間距 12px、跨回合 24px |
| P1 新增 Bot 表單 | `213-ui-newbot-dark` | 欄位順序與預設值文案 |
| P1 設定 drawer | `214-ui-settings-dark` | 右側固定寬度 drawer |
| P2 空狀態、icon/tab/鍵盤 | `215-ui-narrow-900` | 已實作 |

驗證方式：`node` 驅動 headless Chrome（CDP）對真 daemon（127.0.0.1:7788）截圖，主控台零例外。

## 圖片拖放（API.md「圖片附件」，2026-09-06 新增）

**三種放圖方式**，單一 bot 對話與專案群組聊天都支援：

1. **拖放**——放到對話區任何地方（不只輸入框）。整個 `.chat` 是 drop target，拖曳時蓋一層虛線
   veil（`DropVeil`）。`dragenter` / `dragleave` 會對每個子元素各觸發一次，所以 `useDropTarget`
   用計數器記深度，指標掃過氣泡時 veil 不會閃爍。
2. **貼上**——在輸入框 `Cmd+V`。只有剪貼簿真的帶圖片時才 `preventDefault()`，貼文字照常。
3. **📎 按鈕**——輸入框左側，開系統檔案選擇器（`accept="image/*"`，可多選）。

**流程**：檔案一進來就上傳（`POST /bots/:id/attachments`），輸入框上方出現待送縮圖列
（`AttachTray`，上傳中/失敗都有狀態），送出時只帶回傳的 id。上傳未完成時送出鈕顯示「上傳中…」
並停用。送出成功才清空縮圖列。

**已送出的訊息**：user 氣泡下方顯示縮圖（`MessageAttachments`），點開是 lightbox（Esc 或點背景
關閉），底下那行 code 是該圖在 **agent 主機上**的絕對路徑。

**實作位置**：`components/Attachments.tsx`（`useAttachments` / `useDropTarget` / `AttachTray` /
`AttachPicker` / `MessageAttachments`）。附件狀態由 `ChatPanel` / `GroupChatPanel` 持有再傳給
composer——drop 目標是整個對話區，狀態放在 composer 裡就接不到。

**注意**：只收圖片，單檔 12 MB；非圖片會跳通知並略過。縮圖的位元組在 token 之後，所以是 fetch
成 blob 再轉 object URL（`api.attachmentUrl`），同一個 id 全 app 共用一個 URL。

**mock 模式**：`MockTransport.upload` 把位元組留在記憶體，`blobUrl` 直接回傳它，所以
`VITE_MOCK=1` 也能完整走完拖放到縮圖的流程。

驗收截圖（真實 daemon + 真實 claude bot，2026-09-06）：`230-drop-veil-dark`（拖曳中的 veil）、
`231/232-drop-tray`（待送縮圖，深/淺色）、`234-sent-thumb-dark`（已送出的氣泡縮圖）、
`235-lightbox-dark`（放大檢視）、`236-paste-tray-dark`（貼上）。實測 claude 讀得到圖：
問「圖上寫什麼」回「PURPLE 42 / 三隻藍色小鳥」，問背景色回「深海軍藍 #12203C 一類」。
遠端 host（m4p，經 ssh）上傳後遠端檔案 SHA 與本機相同。

**Bot 列的 ⚙ / ⋯（2026-09-06 調整）**：原本是 13px 的 ⚙ / ⋯ 文字符號，字形是髮絲線，壓在選中列
的藍色底上幾乎看不見。改成 `components/Icons.tsx` 的 SVG（`GearIcon` 實線齒輪、`MoreIcon` 三個
實心圓點），16px、`currentColor`，靜置色從 `--text-faint` 提到 `--text-dim`，選中列再提到
`--text`；hover / 選單開啟時是 accent 藍。注意選中列那條規則要寫 `:not(:hover)`——它和 hover
規則 specificity 相同，否則會靠出現順序把 hover 的藍色蓋掉。截圖：`240`/`241`/`242`。

## 回合進行中也能輸入（2026-09-06）

以前一有 in-flight turn，composer 就整個鎖住（黃色「上一則訊息仍在進行中」）。現在改成：

- **輸入框永遠可打字**——`composerState` 把 in-flight 從 `disabled` 拆成新的 `queued`。真正
  不能輸入的情況（未啟動、blocked、主機斷線、delivery unknown）才維持 `disabled`。
- **送出會排隊**——daemon 一個 run 同時只允許一個 in-flight turn（`turns_one_in_flight`
  unique index），直接送會 409。所以 `queued` 時按送出是寫進 store 的 `queuedSends[botId]`
  （每個 bot 最多一則），輸入框與待送圖片照常清空，上方出現藍色「已排隊，這回合結束後送出」
  條，可以按「取消」把文字放回輸入框。
- **回合一結束自動送出**——`turn_updated` 離開 `in_flight` 時呼叫 `flushQueued`。延遲 350ms
  等其他 frame 落地，送出前再檢查一次 `composerState`：若 bot 變成 blocked 或又有新的
  in-flight turn，就保留排隊內容不送，避免吃 409 把訊息弄丟。
- `groupComposerState` 的 `sendable` 要的是「現在就能送」，所以判斷式是
  `!cs.disabled && !cs.queued`——群組送出不排隊，維持原本「送不了就 skip」的語意。

**「輸出中…」移到氣泡下方**（`msg-meta-below`）：一般訊息的 meta 仍在氣泡上方，只有進行中的
LiveBubble 例外——輸出一直往下長，狀態放在成長的那一端才跟得上視線。

驗收（真實 daemon + am-codex，2026-09-06）：`250-live-meta-below-dark`（標籤在氣泡下方）、
`251-queue-typing-dark`（回合中打字，送出鈕變「排隊送出」）、`252/253-queued-strip`（排隊條，
深/淺色）、`254-queue-sent-dark`（回合結束後自動送出，兩則依序抵達）。

**Bot 列的操作鍵（2026-09-06 再簡化）**：⋯ 選單裡原本只有「停止/啟動」和「設定」，而「設定」
旁邊就是 ⚙——同一件事兩個入口。拿掉「設定」後選單只剩一項，選單本身就沒有存在意義了，所以
⋯ 直接換成單一的執行鍵：停著顯示 ▶（`PlayIcon`，hover 綠），跑著顯示 ■（`StopIcon`，hover
紅）。少一次點擊。停止仍會先跳 `ConfirmDialog`（會對 pane 送 ctrl+c），這是把操作從選單搬到
一鍵之後必要的防呆；啟動無害，直接執行。`.bot-row` 的 `menu-open` 隨之更名為 `confirming`
（確認框開著時，圖示不要淡出）。截圖：`260`/`261`（■ 停止，深/淺色）、`262`（▶ 啟動）、
`263`（停止確認框）。

## Bot 列：agent 自己的名字 + 拿掉停止鍵（2026-09-06）

**執行中顯示 agent 取的名字**：bot 名稱旁原本寫「執行中」，現在改顯示 `run.agent_title`
（API.md「run.agent_title」）——claude 會把當前任務寫成標題，所以那行直接告訴你這隻 bot 正在
忙什麼。斜體、`--text-faint`、可截斷（標題常常很長），tooltip 給全文。

只有 `working` / `idle` 時才用標題取代狀態字：`blocked` / `starting` / `stopping` 是使用者要
處理的狀態，那些字不能被蓋掉。沒有標題（或標題被後端判定為預設值）就回到原本的行為。

**拿掉側欄的停止鍵**：平常不會需要停止 bot，一顆常駐的 ■ 只是佔位置又容易誤觸。現在執行中的
列只有 ⚙；沒在跑的列才有 ▶ 啟動。要停止就進那隻 bot 的對話，用標題列的「停止」（那裡本來就
有確認框）。連帶移除 `StopIcon`、`ConfirmDialog` 的 import 與 `.bot-row.confirming`。

截圖：`270-agent-titles-dark` / `271-agent-titles-light`。

## 對話上方的 statusline（2026-09-06）

點進某隻 bot 的對話時，標題列下方多一條 `.statusline-bar`，顯示 `run.status_line`——就是
那隻 bot 在 pane 底下看到的同一行（帳號、專案、模型、5h/7d、F5…，內容取決於使用者自己的
`statusLine` 腳本）。等寬字、單行、過長時橫向捲動（捲軸隱藏），tooltip 給全文。

**改用原始欄位（2026-09-06 同日修訂）**：原本直接顯示腳本輸出的原文，但那行是為終端寬度寫的
——使用者的腳本把 email 截成前 5 碼（`hunta`）、模型縮成 `OP5`，還放不下 context。網頁沒有這個
限制，所以改成讀 `run.status`（API.md `run.status_json`）自己排：完整帳號、完整模型名（含
effort / thinking / fast）、context（% 與 tokens/容量）、5h、7d、花費、版本。原文退居 tooltip，
也是 payload 還沒到時的 fallback。百分比要 round——claude 送的是 `28.000000000000004`。

標題列的額度條是另一回事：它是跨 bot 的總覽，這條是這隻 bot 自己的。

沒有 statusline 的 bot（codex / grok，或沒設定 `statusLine.command`）不會出現這條，不留空位。
截圖：`281-statusline-head-dark` / `282-statusline-head-light`。

**codex / grok 也有 statusline（2026-09-06）**：codex 的狀態列是 TUI 內建的，欄位由
`~/.codex/config.toml` 的 `[tui] status_line` 決定（這台是 `model-with-reasoning`、
`current-dir`、`model`、`five-hour-limit`、`weekly-limit`）；grok 同理。兩者都沒有 claude 那種
可以把文字交出來的 statusLine command，而從 pane 讀回來的又是被終端寬度截斷的版本
（`gpt-5.6-luna max fast · ~/…`）。

這些欄位 store 裡本來就有，所以改用 `derivedStatus()` 直接組：模型（`bot.model` + effort +
fast）、目錄（`project.path`，家目錄縮成 `~`）、5h / 7d（`quota[kind:identity]`，ISO 時間換成
epoch 秒好和 claude 的欄位共用同一個 renderer）。沒有帳號就把目錄提到第一格。claude 仍走
`run.status`（它的資料更多，還有 context 和花費）。

**點模型改（2026-09-06）**：狀態列的「模型 …」和標題列的模型標籤都是按鈕，開出
`ModelQuickPicker`（模型清單，三種 kind 現在都有強度）。選了就 `PATCH /api/bots/:id`。
claude / grok 執行中會走 TUI slash 指令當場套用（grok `/model <id> [effort]`、`/effort`；
claude `/model`、`/effort <level>`）；codex 仍要重啟。

grok 目前 `quota.grok` 是 null（`/usage` 探測讀不到 pane），所以它的那條只會有目錄與模型。

## 強制中止一個回合（2026-09-07 加）

輸入框被鎖住時（回合還在跑，或上一回合送達狀態未知），那一條 `composer-lock` 除了原本的
「放棄該回合」「中斷回覆」之外多一顆紅框的 **強制中止**：

- 「中斷回覆」是請 agent 停（`POST /bots/:id/interrupt` → 送 `esc`）。`esc` 送不進去時
  ——pane 沒了、herdr 斷線、agent 不理——那支會 502，回合仍卡在 in-flight，輸入框繼續鎖著。
- 「強制中止」走 `POST /bots/:id/abort`（`store.abortBot`）：先解鎖，送鍵只是順帶。回應的
  `keys_sent` 為 false 時前端用 error 色的 notice 講清楚「agent 那頭可能還在跑，必要時停掉 Bot」，
  不會假裝什麼都好了。
- in-flight 與 `delivery=unknown` 兩種卡法都收，所以使用者不必先按「放棄該回合」再按一次別的。
- 按鈕在請求期間顯示「中止中…」並 disabled（`busy['abort:<botId>']`）。

## Team 成員的身分（2026-09-07 加）

一個 team 常常一個角色一個帳號（分散額度），所以 `identity` 要看得見，不能只留在 tooltip：

- **主區的成員 chip**（`TeamPanel.MemberChip`）：短名徽章（`pm` / `dev-1` / `rev`）後面接
  `IdentityBadge`，沒指定就顯示「預設」——和側欄一般 bot 列同一顆元件、同一條規則。
- **側欄 team 節點下的成員列**（`TeamNodes.MemberRow`）：`bot-sub` 那行的 `KindTag` 後面同樣接
  `IdentityBadge`。
- 兩邊的 tooltip 都多一行「身分 cc2 / 預設」。
- 開團表單本來就能逐角色選身分（`TeamLaunchPanel` 用的就是 `IdentityOptions`，吃專案的 host，
  所以 shell 認來的 `ccN` 也在選項裡，SPEC §16）。
- 驗收 `node scripts/demo-team-identity.mjs`（截圖 `357-team-member-identity.png`）：PM 指定 cc2、
  其餘留預設，建立後主區是 `pm cc2 ｜ dev-1 預設 ｜ dev-2 預設 ｜ rev 預設`，側欄同步。
  過程中順手補了 mock 的一個落差：`createTeam` 只讀舊的 `issue_number`，而前端早就送
  `issue_numbers` 佇列（SPEC-team §2.3），所以 mock 一直回 `gh: issue #0 not found`。

## Team 完成後的「關閉 issue」提議（SPEC-team §10.7）

`TeamPanel` 在 `phase === 'done'`、`issue_closed_at` 為 null、且該專案有 GitHub origin 時，在 PM 總結
下面顯示一列提議（`.team-close-issue`）與一顆「關閉 issue #N」按鈕；按下去先出 `ConfirmDialog`
（說明會留什麼留言、以及分支還沒合併進 base 這件事），確認後才呼叫 `POST /teams/:id/close-issue`。

- **不做自動關閉**：這一列是提議，不是進度；daemon 端也拒絕在非 `done` 的 team 上關 issue。
- 關掉後標題列的 issue 連結變成「issue #N · 已關閉」，提議列消失（`issue_closed_at` 有值）。
- 專案沒有 GitHub origin 時整列不顯示（後端會回 400，但使用者不該先看到一顆按不動的按鈕）。

### timeline 不再吞掉沒有 `text` 的 note

`describeEvent()`（`components/teamPanelLogic.ts`）原本只認 `payload.text`，所以 `member_start_failed` / `pretrust_failed` /
`protocol_error` / `issue_closed` 這些帶結構化欄位的 note 會回 `null`，整列不渲染——成員啟動失敗時
畫面上只剩一個灰燈與「已暫停：成員已離線」，原因明明就在 team 日誌裡卻看不到。現在四種 action 各給
一句人話，其餘 note 至少顯示 `action` 與可讀欄位。

## done 的 Team 追加 issue 繼續（SPEC-team §2.5，2026-09-07 加）

跑完但還沒清理的 team 現場都在（`main/` 與 `reviewer/` worktree、PM 對話、整合分支），
所以「順便把 #57 也做了」不必重新組隊。

- **done 卡片**多一列 `.team-reopen-issue` 與一顆「追加 issue 繼續」：展開一個輸入框，
  issue 號以逗號或空白分隔（`#57, 58` 也吃），送 `store.addTeamIssues`。
- **顯示條件**是 `canReopenTeam(team, unavailable)`（`components/teamPanelLogic.ts`）：
  `phase === 'done'` 且 `members` 裡 role `pm` 那一筆的 `deleted` 不為 true。cleanup / delete
  會把成員軟刪除，daemon 的 `team_json` 用 `deleted` 把這件事送上來。
- daemon 回 `409 {"reason":"team is cleaned up"}` 時記進 `store.teamReopenUnavailable[teamId]`
  並 toast，按鈕當場收掉——那個 409 是永久的，讓使用者再按一次沒有意義。
- 送出後 `team_changed` 把 phase 推成 `starting` → `planning`，面板自動切回進行中視圖
  （composer 解鎖、成員 lamp 亮起）。`done` 的 composer-lock 文字也改成講這條路。
- **`IssueQueue` 每一列**在 `state === 'done'`、`issue_closed_at` 為 null 且專案有 GitHub origin
  時給一顆「關閉 issue」小按鈕，走跟 done 卡片同一個 `ConfirmDialog`，但帶
  `closeTeamIssue(teamId, issue.id)` → body 的 `issue_id`（§2.5.4）。沒有它的話，reopen 之後
  `teams.issue_number` 已經換成新 issue，上一個 issue 就再也關不掉了。
- **timeline** 兩則新 note：`team_reopened`（「使用者追加 #57、#58，team 重新啟動」）與
  `member_context_lost`（「PM 沒能續接先前對話，已改為新對話」）。`done → starting` 那則 phase
  事件的 reason `reopen` 也在 `TEAM_PAUSE_LABEL` 裡翻成「使用者追加 issue」——它不是暫停原因，
  但時間軸用同一張表翻譯 reason。
- 單元測試 `node --test --experimental-strip-types src/components/teamPanelLogic.test.ts`（3 項）。
  mock（`VITE_MOCK=1`）對 `done` 的 team 收下追加、推成 `starting`，1 秒後 `planning` 並補上
  上述兩則 note 與 `next_issue` relay，整段流程看得到。

## 圖片暫存托盤（跨對話，2026-09-07 加）

手上有一張圖、但想給的是**另一隻** bot：以前只能先切過去再拖，因為附件托盤是每個草稿自己的
（換 bot 就重新掛載）。現在畫面多了第三格「圖片暫存」——**不屬於任何對話**的暫存區。

**位置**：`.app` grid 的第三欄。桌機在右緣（收合 38px 直立握把，展開 208px），`≤1024px`
變成底部一條橫向捲動的托盤（`grid-template-rows: 1fr auto`）。是 grid 的一格、不是浮動面板，
所以永遠不會蓋住訊息或輸入框。元件 `<ImageShelf />` 掛在 `App.tsx` 的 `main` **外面**，
換 bot / project / team 都不會 unmount，圖片自然跨得過去。收 / 展記在 `localStorage`
（`am.shelf.open`）；圖片本身鏡射到 IndexedDB（見下）。

**狀態**：`store/shelf.ts` 自己一個 zustand store（`useShelf`），刻意不進主 store：主 store
會把草稿鏡射到 localStorage、`refreshState` 會整批換掉 slice，`File` 與 object URL 兩者都撐不過。
每張是 `{ key, file, name, size, url, addedAt }`，`url` 是 `createObjectURL`，移除 / 清空時 revoke。
`store/shelfPersist.ts`（`main.tsx` 啟動時呼叫 `startShelfPersistence()`）把每張 `File` 存進
IndexedDB `am-shelf`，載入時讀回並 `restore()`，之後 subscribe store 差異做 put / delete；
從放進去起 `SHELF_TTL_MS`（30 分鐘）後 `expire()` 清掉，載入時與每分鐘各掃一次。IndexedDB
不能用時靜默退回純記憶體。daemon 不知道有這一層。單檔 12 MB（`MAX_BYTES`，與 `attach::MAX_BYTES`
同一個數字，現在由 `shelf.ts` 擁有、`Attachments.tsx` 反過來 import），張數上限 24
（`SHELF_MAX`，暫存區會一直活著，需要一條記憶體護欄）。

**進**：拖到暫存區、暫存區自己的 ＋ 檔案選擇器（手機唯一入口）、暫存區內有 focus 時貼上。
**這三條路都只 cache、不上傳**——附件 id 綁收件 bot 的 project（`attach::resolve`），
先上傳給誰都是錯的。

**出**（上傳就發生在這一刻，對著使用者真的選的那隻 bot）：

- **點一下縮圖** → 進「目前這個對話」的附件托盤。哪個對話算目前的，由掛著的
  `ChatPanel` / `GroupChatPanel` 用 `useShelfSink(files.add, 名稱)` 註冊到 shelf store；
  ChatPanel 在終端分頁時不註冊（那時托盤不在畫面上，圖會像憑空消失）。沒有 sink（Team 面板、
  還沒選 bot）時給一則 notice，不會默默吞掉。
- **拖進對話**（桌機）：item `draggable`，`dataTransfer` 只帶自訂 mime
  `application/x-am-shelf`（值是 shelf key）。`useDropTarget` 認這個 type，drop 時用 key
  回 shelf 取 `File` 再走原本的 `files.add`——所以兩個 panel 的 drop 接線一行都沒改。
- **鍵盤**：卡片是 `<button>`，Enter / Space 放進目前對話，Delete / Backspace 移除。

**放大預覽（2026-09-07 追加）**：卡片只有 96px 寬，夠分辨兩張截圖、不夠看內容，所以 hover /
focus（觸控是**長按** 450ms）會浮一張 300px 的大圖 + 檔名。刻意放在托盤**外面**——桌機貼著
窄欄左緣、底部托盤時在整條托盤上方（不是只避開卡片：只避開卡片會蓋到標題列的 ＋ / ✕ / »）
——而且 `pointer-events: none`，所以連攔都攔不到卡片自己的移除鍵。位置在 layout effect 裡
量完自己再放（一次繪製到位），高度受 `--peek-avail`（上方真正剩下的空間）壓制，橫著拿的手機
是圖片變小、不是預覽被夾回托盤上面。hover 有 180ms 延遲（滑過一整排不會每張都閃），
focus 立即；`Escape`、捲動、resize、拖曳開始都會收掉。

觸控上點一下本來就是「放進對話」，不能拿去做預覽，否則手機就沒有主要動線了：所以長按開預覽，
長按產生的那次 click 被吞掉，預覽開著時再點一下是收起來（`ShelfCard` 的 `swallowClick` /
`peek === 'touch'`）。`.shelf-card-main` 因此要關掉 iOS 的 callout 與文字選取。

**複製語意，不是搬移**：放進對話後暫存區仍留著（同一張截圖常要餵好幾隻 bot），只在卡片上閃
一下綠色「已放入」。要清掉自己按 × 或標題列的 ✕。誤觸不會弄丟圖。

**收合時的 drop pad**：38px 的握把在拖著檔案時根本瞄不到，所以偵測到視窗上有檔案拖曳
（window 層的 `dragenter`/`dragleave` 計數）就在握把旁浮出一塊虛線 pad。它是 `position: absolute`
——把 grid 欄位撐寬會在指標底下 reflow 整個對話區。手機沒有檔案拖曳，那塊直接 `display: none`。

**驗收**（真 daemon，headless Chrome 走完整條路）：兩張圖 drop 進暫存 → 開 bot A 點第一張
（tray 出現、上傳完成、暫存**仍是 2 張**）→ 換 bot B（暫存還在、B 的 tray 是空的）→ 從暫存
拖第二張進 B 的對話（`dataTransfer` 只有 `application/x-am-shelf`，key=s2，上傳成功）→
卡片 focus 後 Enter 再放一張、Delete 移除一張 → 淺色、收合握把、拖曳中的 pad → 390px
手機底部托盤（點一下放進對話）與收合列。

## 已完成（未讀）（2026-09-07 加）

回合狀態多一段中間值：**進行中 → 已完成（未讀）→ 已完成（已讀）**。取捨寫在
`docs/UI-DECISIONS.md`；這裡是實作與驗收。

**檔案**：`web/src/store/unread.ts`（純函式 ＋ localStorage，附 `unread.test.ts`）。
`store.ts` 只掛最小的 hook，daemon 完全沒有改動——未讀是「這個瀏覽器的人看過什麼」，
協定裡沒有它的位置。

**記帳的時機**（`store.ts` 的 `handleFrame`）：

- `message_added` 的 assistant 訊息、以及 `turn_updated` 進終態，都算「一個回合完成」。
  同一個回合兩者都會來，`takeTurnCompletion(botId, turnId)` 讓它只跳一次；沒有 assistant
  訊息的回合（中止、只有終端輸出）靠後者才不會漏掉。
- 完成的當下如果**不是**「正在看它」就 +1。「正在看」＝ `selectedBotId` 是它、而且沒有群組 /
  team / shell 蓋在上面、而且 `document.visibilityState === 'visible'`、而且
  `document.hasFocus()`（`windowActive()`）。
- 同一則回覆也記一份在它專案的群組聊天上（`groupUnread`，§13.6 本來就有，現在跟著同一套
  可見性規則走，而且會存下來）。team 成員就是 bot，走的是同一條路，不必另外處理。

**清成已讀**：`selectBot` / `selectProject`（視窗在前景時）、以及 `App.tsx` 的
`useUnread()` 在 `focus` / `visibilitychange` 時把「現在開著的那個」標成已讀。

**畫面**：側欄 bot 列在狀態燈右邊加 `.unread-turns`（`!2`，accent 方角小標）；專案收合時
把底下所有 bot 的未讀加總掛在專案標題上；分頁標題掛 `(N)` 前綴（只算 bot 那一邊，
群組的是同一批回覆的第二份帳）。

**localStorage**（都用 try/catch 包住，無痕視窗只是不跨重整）：

| key | 內容 |
| --- | --- |
| `am.readMarks` | `{"bot:<id>" \| "group:<projectId>": {at, id}}` — 最後已讀的訊息時間與 id |
| `am.unread` | `{"bot:<id>" \| "group:<projectId>": n}` — 未讀回合數的快照 |

兩份都存的理由：重整後只有正在看的那個 bot 會載入訊息，其他的一則都沒有，光靠標記算不出
數字；而光靠數字會跟真實訊息漂移。訊息真的載進來時 `recountBot` 用標記重算一次校正。

**一個回合只跳一下**：同一個回合會先來 `message_added`（assistant）再來 `turn_updated`
（終態），兩個都算「完成」，所以 `takeTurnCompletion` 去重。兩邊必須用**同一個 key**：
`turn_updated` 用的是 `turn.id`，而沒有帶 `turn_id` 的訊息由 `completionKey()` 掛到這個 bot
最近的那個回合上（退回 `msg:<id>` 只在連一個回合都還不知道時）。用 `msg:<id>` 記第一次、
`turn.id` 記第二次的話，一則回覆會讓徽章加二。
每次 `GET /api/state` 之後 `pruneUnread()` 把已經不存在的 bot / project 的帳丟掉。

**驗收**：

- `cd web && node --test --experimental-strip-types src/store/unread.test.ts`（15 項）。
- `node scripts/demo-unread.mjs`（mock @ 5311）：送出後切到別的 bot → 那一列出現 `!1`、
  分頁標題 `(1)`；收合專案 → 標題掛上 `!1`；點回去 → 徽章與 `(N)` 都清掉，
  `am.readMarks` 推到最後一則。截圖 `docs/screenshots/unread/440`、`441`、`443`。
- `node scripts/demo-unread-persist.mjs`（真 daemon @ 5173，只讀畫面 ＋ 寫 localStorage，
  不對任何 bot 送訊息）：帳本寫進 localStorage → 重整後徽章 `!3`、標題 `(3)` 還在；
  把分頁改成 `hidden` ＋ `hasFocus()=false` 再點進那個 bot → **不會**清掉（並且
  `recountBot` 用假的 2020 年標記從真實訊息重算出整串歷史的回合數）；改回 visible 並丟一個
  `visibilitychange` → 立刻清成已讀。截圖 `docs/screenshots/unread/444`–`446`。


## 執行者那一格 = 併行數，task 列有「排隊中」（2026-09-08）

`TEAM_WORKERS_DEFAULT` 由 2 改成 **1**（`api/types.ts`）。`TeamLaunchPanel` 的執行者卡：
stepper 的單位由「人」改成「個」，`aria-label` 改成「併行數」／「增加（減少）一個併行位」，
hint 改成「最多同時跑幾個 task；PM 派幾筆都可以，多的排隊。每個併行位一個獨立 worktree 與分支」。
名字與取捨的理由寫在 `docs/UI-DECISIONS.md`。

`TeamTask.worker_bot_id` 現在是 `string | null`（`normalize.ts` 改用 `optStr`），`null` = 還在排隊。
`TeamPanel` 的 task 列因此分兩種：

- 有執行者 → 照舊顯示短名；
- `null` → `<span className="team-task-who queued">排隊中</span>`（`.team-task-who.queued`
  比 `--text-dim` 再淡一階），並帶 title「還沒有執行者接手，有人空下來就會自動派出」。

標題列的 `disclosure-note` 前面多一段 **`併行 M/n`**：`n` 是 team 裡 `team.role === 'worker'`
的 bot 數，`M` 是有執行者且未進終態（`TASK_TERMINAL = merged | skipped | failed`）的 task 數。
「3 個 task」不再等於「3 個在跑」，所以這個比值要獨立畫出來。

`api/mock.ts` 的示範 team 多派一筆（`workers.length + 1`）：它以 `worker_bot_id: null` 進場，
在第一筆合併之後才被指派、切分支、開始跑——mock 走的就是 daemon 的補位順序。

---

## 路由：每個畫面都有自己的 URL（2026-09-09）

以前整個 UI 只有 `/`，選了哪個 bot / project / team 都只是 store 的狀態：重新整理回到首頁、
上一頁沒作用、也沒辦法把「這個 bot 的對話」貼給別人或加到手機主畫面。現在每個畫面有 path：

| 畫面 | path |
|---|---|
| 母 bot / 子 bot 對話 | `/bots/:botId` |
| 母 bot 終端分頁 | `/bots/:botId/terminal` |
| Bot 設定（浮窗） | `/bots/:botId/settings` |
| Project 群組聊天 | `/projects/:projectId` |
| 組隊（`TeamLaunchPanel`） | `/projects/:projectId/teams/new?issue=<n>` |
| Team | `/teams/:teamId` |
| 主機 shell | `/hosts/:host/shells/:paneId` |
| 首頁（沒選東西） | `/` |

**沒有 router 套件**。`lib/routes.ts` 是一對純函式（`parseRoute` / `buildRoute` / `screenKey`），
`store/routeSync.ts` 用 `history.pushState` + `popstate` 把它跟 store 接起來。元件完全不知道
這件事：側欄與所有「開啟 X」的按鈕維持原本的 `onClick`（走 store），**網址是 store 的投影**。

### 三條規則

- **store → 網址**：`routeOf(state)` 依 `App.tsx` 的 render 分支順序（shell > 組隊 > team >
  project > bot）算出目前的畫面。`screenKey` 一樣就 `replaceState`——對話↔終端不該讓上一頁
  多一格；不一樣就 `pushState`。設定浮窗刻意算成另一個 `screenKey`：開著時 push，所以「關掉」
  就是上一頁。
- **網址 → store**：`popstate` 解析路徑後呼叫對應的 `selectBot` / `selectProject` / `selectTeam`
  / `openTeamLaunch` / `viewHostShell`。找不到那個東西（bot 被刪、shell 被關）就回首頁並
  `notify`——連結會過期，靜靜停在一個空畫面比說出來更難懂。
- **開頁**：先 `parseRoute(location.pathname)`，但**等 `ready`** 才套用（要先有 `GET /api/state`
  的清單才判斷得出「還在不在」）。網址是 `/` 時尊重 store 從 localStorage 還原的選取，不清空，
  再把它 `replaceState` 成真正的路徑；localStorage 的選取記憶因此仍然有效，只是多了個網址。

### 幾個邊角

- **token**：`?token=` 只在第一次載入用（`transport.ts` 拿到 `GET /api/session` 就自己快取）。
  套用路由時的第一次 `replaceState` 會把它從網址上拿掉，分享出去的連結不帶憑證。
- **手機抽屜**：開的時候借一格歷史（URL 不變，只在 `history.state` 上記 `{am:'drawer'}`），
  按上一頁就是關抽屜、不換畫面。在抽屜裡點了會換頁的東西時不必特別處理：store 的訂閱比
  React 的 re-render 早跑，`syncNow` 看到自己站在 `drawer` 那一格就改用 `replaceState`
  把它換掉。用 ✕ / scrim / Esc 關的則 `history.back()` 把那一格還回去。
- **`repo` 不進網址**：`/projects/:id/teams/new?issue=<n>` 只帶 issue 編號。從連結進來一律
  當專案本身的 repo，submodule 的組隊還是要從 Issues 列表點進去（那裡才知道是哪個 submodule）。
- **`document.title`** 跟著畫面走：`C1-fable · Agents Manager`、`#48 … · Team · Agents Manager`、
  `mini · shell · Agents Manager`；未讀數仍然掛在最前面（`(3) C1-fable · Agents Manager`）。
- **SPA fallback**：`daemon/src/assets.rs` 對任何路徑回 `index.html`，`index.html` 引的是絕對
  路徑（`/assets/…`、`/favicon.svg`），所以 `/bots/x/terminal` 這種多層路徑照樣載得到。
  Vite dev（`appType: 'spa'` 預設）也一樣，`http://127.0.0.1:5173/bots/<id>` 直接開得起來。

### 驗證

- `web/src/lib/routes.test.ts`：parse/build 對稱、壞路徑回首頁、`?issue=` 保留、id 逸出。
- `scripts/ui-routes-shots.mjs`（headless Chrome，對 5173 的真 daemon）：直接開 `/bots/<id>`、
  點終端、上一頁、設定開關、`/teams/<id>` reload、壞連結、手機抽屜的上一頁；截圖在
  `docs/screenshots/routes/`。跑法：`BOT=<id> TEAM=<id> PROJECT=<id> OUT=dir node scripts/ui-routes-shots.mjs`。

## 無限併行（`workers.count = 0`，2026-09-09）

SPEC-team §4.5。取捨寫在 `docs/UI-DECISIONS.md`「無限併行」，這裡只記程式在哪。

- `api/types.ts`：`TEAM_WORKERS_UNLIMITED = 0`、`TEAM_MAX_CONCURRENT_ISSUES = 6`、
  `TEAM_MAX_TEAM_WORKERS = 12`；`TeamRolePatch.count`；`TeamRoles.workers_count`
  （`roles_json.workers.count`，**`0` 要留著**，別用 `|| null` 吃掉）。
- `api/normalize.ts`：`toTeamRoles` 帶出 `workers_count`。
- `TeamLaunchPanel`：執行者卡的 stepper 旁多一顆「∞」切換（`is-on` 樣式），hint 與送出鍵
  的文案跟著換。它不是「4 再加一」，所以是獨立按鈕而不是 stepper 的一格。
- `TeamRoleEditor`：執行者的表單多一排併行數（∞ / 1–4），改了才送 `count`；`applied` 由
  daemon 回（改成 0 一律 `now`）。收合摘要用 `∞` 取代 `×n`。
- `TeamPanel`：
  - `IssueQueue` 的「當前」判斷改成 `i.state === 'working'`（可以有好幾列），摘要多「進行中 N」。
  - `TaskSection` 分岔：>1 個 working 走 `TaskGroupByIssue`（每個 issue 一段，標整合分支與
    `∞ · 執行者 k · 進行中 m/n`），否則走原本的 `TaskList`。兩者共用 `TaskRows`。
  - `MemberStrip` 在多 issue 時把執行者按 issue 分段。issue 位置要從 **`bot.name`** 讀
    （`issueSeqOfMember`）——`teamShortName` 的工作就是把 `i<seq>-` 拿掉，拿短名配對永遠是 null。
- `teamProgress.ts`：`workingIssuesOf` / `TeamProgress.workingIssues`；`issueQueueAt` 把所有
  working 都算進「已開工」；計時取**最早**開跑的 working issue。`TeamIssueProgress` 在
  `workingIssues > 1` 時改印「進行中 N」。
- `api/mock.ts`：`count = 0` 時把佇列裡的 issue 全部設成 `working`、各給一個 `i<seq>-dev-1`，
  task 帶 `issue_id`，這樣 mock 就能重現多 issue 的畫面（截圖就是這樣拍的）。
